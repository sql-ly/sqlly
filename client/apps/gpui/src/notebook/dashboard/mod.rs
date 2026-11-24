//! The dashboard surface of a notebook (spec 8.3): a layout cell's sections
//! and tabs drawn as a responsive 12-column canvas whose tiles show other
//! cells' outputs, with a filter bar bound to the notebook's parameters.
//!
//! Two experiences share the canvas. The editor adds tiles from a picker
//! (click or drag), resizes them with edge handles, reorders them by drag or
//! arrows, and edits titles, notes, visibility rules and drill-throughs.
//! The viewer shows only outputs (never SQL, never an editor) and offers
//! filters, drill-throughs and Refresh, which names the queries and
//! connections it is about to run before anything runs. Presentation mode
//! (`present/`) draws the same canvas one section tab at a time.
//!
//! The dashboard is stored in the layout cell's `spec`
//! (`sqlly_services::notebook::cells::layout`); every edit here writes it
//! back, so it saves, autosaves and undoes like any other cell change.

#[cfg(test)]
pub(crate) mod tests;

use std::cell::Cell as StdCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, AppContext as _, Context, CursorStyle, Entity, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, prelude::FluentBuilder as _,
    px, relative,
};
use gpui_component::Disableable as _;
use gpui_component::Icon;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use sqlly_core::{CellValue, ConnectionEnvironment};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::notebook::CellKind;
use sqlly_services::notebook::cells::layout::{
    self, Breakpoint, Condition, Drill, LayoutSpec, ROW_HEIGHT, SlideRef, Tile, Visibility,
    VisibilityInputs,
};

use super::cells;
use super::doc_adapter::{KIND_MARKDOWN, KIND_SQL, RunScope};
use super::params::widget::{ParamWidget, Surface as WidgetSurface};
use super::rails::{icon_button, text_button};
use super::view::{CellStatus, NotebookView};
use crate::icon_catalog::AppIcon;
use crate::theme::Theme;

/// Cells a starter dashboard places.
pub const STARTER_TILES: usize = 6;
/// Values a drill-through menu lists.
pub const DRILL_CHOICES: usize = 20;
/// Gutter around each tile, in unscaled pixels.
const TILE_GUTTER: f32 = 6.0;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

/// A cell value as the dashboard shows it in filters and drill menus.
pub fn value_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(_) => String::new(),
    }
}

/// What a tile body shows for a cell. SQL is never one of them: a SQL
/// cell's tile is its result grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileBody {
    /// The result grid of a SQL cell.
    Grid,
    /// Rendered Markdown, read-only.
    Text,
    /// The kind's own output renderer.
    Output,
    /// The tile names a cell that is gone.
    Missing,
}

pub fn tile_body(kind: Option<&str>) -> TileBody {
    match kind {
        None => TileBody::Missing,
        Some(KIND_SQL) => TileBody::Grid,
        Some(KIND_MARKDOWN) => TileBody::Text,
        Some(_) => TileBody::Output,
    }
}

/// A tile edge being dragged.
#[derive(Debug, Clone, PartialEq)]
pub struct TileResize {
    pub tile: String,
    pub width: bool,
    pub start: f32,
    pub start_value: u8,
    /// Pixels per grid column (width) or per row (height).
    pub unit: f32,
}

/// What a drag from the picker or a tile carries.
#[derive(Debug, Clone)]
pub enum DraggedTile {
    Cell(String),
    Tile(String),
}

struct DragLabel(String);

impl Render for DragLabel {
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

/// The queries a refresh would run, and where.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefreshPlan {
    pub cells: Vec<String>,
    /// Connection labels, each once, in first-use order, with whether it
    /// is a production connection.
    pub connections: Vec<(String, bool)>,
}

/// Per-tab dashboard state. The dashboard itself lives in the layout cell.
pub struct DashboardState {
    pub cell: String,
    pub editing: bool,
    pub slide: SlideRef,
    pub picker: bool,
    pub selected_tile: Option<String>,
    tile_inputs: Option<(String, Entity<InputState>, Entity<InputState>)>,
    title_inputs: Option<(SlideRef, Entity<InputState>, Entity<InputState>)>,
    pub resize: Option<TileResize>,
    width: Rc<StdCell<f32>>,
    /// Result grids of SQL tiles, with the output version they show.
    grids: HashMap<String, (u64, Entity<SqllyDataTable>)>,
    /// Set after a filter or drill-through makes shown tiles out of date.
    pub refresh_offer: bool,
}

impl DashboardState {
    pub fn new(cell: String, editing: bool) -> Self {
        Self {
            cell,
            editing,
            slide: SlideRef { section: 0, tab: 0 },
            picker: false,
            selected_tile: None,
            tile_inputs: None,
            title_inputs: None,
            resize: None,
            width: Rc::new(StdCell::new(1200.0)),
            grids: HashMap::new(),
            refresh_offer: false,
        }
    }

    pub fn breakpoint(&self) -> Breakpoint {
        Breakpoint::for_width(self.width.get())
    }

    #[cfg(test)]
    pub fn set_width_for_test(&self, width: f32) {
        self.width.set(width);
    }
}

impl NotebookView {
    // ── Model ─────────────────────────────────────────────────────────────

    /// Layout cells in document order with their display names.
    pub fn layout_cells(&self) -> Vec<(String, String)> {
        self.doc
            .cells()
            .filter(|c| c.kind == CellKind::Layout.as_str())
            .map(|c| (c.id.to_string(), c.display_name().to_string()))
            .collect()
    }

    /// The dashboard in layout cell `id`.
    pub fn layout_spec(&self, id: &str) -> Result<LayoutSpec, String> {
        let spec = self
            .doc
            .model()
            .cell(id)
            .map(|c| c.spec.clone())
            .unwrap_or_default();
        LayoutSpec::from_value(&spec).map_err(|e| crate::localization::message(e.message()))
    }

    pub(crate) fn dashboard(&self) -> Option<&DashboardState> {
        self.present.dashboard.as_ref()
    }

    pub(crate) fn dashboard_mut(&mut self) -> Option<&mut DashboardState> {
        self.present.dashboard.as_mut()
    }

    fn current_spec(&self) -> Option<LayoutSpec> {
        let dash = self.dashboard()?;
        self.layout_spec(&dash.cell).ok()
    }

    /// Change the open dashboard (editor only). Returns whether it changed.
    pub fn edit_layout(
        &mut self,
        change: impl FnOnce(&mut LayoutSpec) -> bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dash) = self.dashboard() else {
            return false;
        };
        if !dash.editing {
            return false;
        }
        let id = dash.cell.clone();
        self.change_layout_cell(&id, change, cx)
    }

    /// Change the dashboard in layout cell `id` from the notebook (its cell
    /// body) or the editor.
    pub(crate) fn change_layout_cell(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut LayoutSpec) -> bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Ok(mut spec) = self.layout_spec(id) else {
            return false;
        };
        if !change(&mut spec) {
            return false;
        }
        if self.doc.set_spec(id, spec.to_value()) {
            self.mark_dirty(cx);
            true
        } else {
            false
        }
    }

    /// Give an empty layout cell a first dashboard from the notebook's
    /// result cells.
    pub(crate) fn set_up_dashboard(&mut self, id: &str, cx: &mut Context<Self>) {
        let starter = LayoutSpec::starter(self.doc.model(), STARTER_TILES);
        self.change_layout_cell(
            id,
            |spec| {
                if spec.sections.is_empty() {
                    *spec = LayoutSpec {
                        extra: std::mem::take(&mut spec.extra),
                        ..starter
                    };
                    true
                } else {
                    false
                }
            },
            cx,
        );
    }

    // ── Opening and modes ─────────────────────────────────────────────────

    /// The layout cell to open: `id`, else the first one, else a new one
    /// appended to the notebook with a starter dashboard.
    pub(super) fn layout_cell_or_create(
        &mut self,
        id: Option<String>,
        cx: &mut Context<Self>,
    ) -> String {
        if let Some(id) = id.filter(|id| self.doc.cell_by_id(id).is_some()) {
            return id;
        }
        if let Some((first, _)) = self.layout_cells().into_iter().next() {
            return first;
        }
        let end = self.doc.len();
        let id = self.insert_cell(CellKind::Layout.as_str(), end, cx);
        self.set_up_dashboard(&id, cx);
        id
    }

    /// Open the dashboard editor on layout cell `id` (or the notebook's
    /// first dashboard, creating one when there is none).
    pub fn open_dashboard_editor(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let id = self.layout_cell_or_create(id, cx);
        self.enter_dashboard(id, true, cx);
    }

    /// Open the viewer. Refused (with the reason shown) when the notebook
    /// has cells that change data.
    pub fn open_dashboard_viewer(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if let Err(refusal) = self.publish_check(
            sqlly_services::notebook_security::authorize::PublishMode::InteractiveApp,
        ) {
            self.refuse(refusal, cx);
            return;
        }
        let id = self.layout_cell_or_create(id, cx);
        self.enter_dashboard(id, false, cx);
    }

    pub(crate) fn enter_dashboard(&mut self, id: String, editing: bool, cx: &mut Context<Self>) {
        let keep = self
            .dashboard()
            .filter(|d| d.cell == id)
            .map(|d| (d.slide, d.grids.clone()));
        let mut state = DashboardState::new(id, editing);
        if let Some((slide, grids)) = keep {
            state.slide = slide;
            state.grids = grids;
        }
        self.present.dashboard = Some(state);
        self.present.surface = super::present::Surface::Dashboard;
        self.present.refusal = None;
        self.clamp_slide();
        self.request_focus_root();
        cx.notify();
    }

    pub fn set_dashboard_editing(&mut self, editing: bool, cx: &mut Context<Self>) {
        if !editing
            && let Err(refusal) = self.publish_check(
                sqlly_services::notebook_security::authorize::PublishMode::InteractiveApp,
            )
        {
            self.refuse(refusal, cx);
            return;
        }
        if let Some(dash) = self.dashboard_mut() {
            dash.editing = editing;
            dash.picker = false;
            dash.resize = None;
            dash.tile_inputs = None;
            dash.title_inputs = None;
        }
        cx.notify();
    }

    pub fn close_dashboard(&mut self, cx: &mut Context<Self>) {
        self.present.surface = super::present::Surface::Notebook;
        if let Some(dash) = self.dashboard_mut() {
            dash.resize = None;
            dash.picker = false;
        }
        self.request_focus_root();
        cx.notify();
    }

    fn clamp_slide(&mut self) {
        let Some(spec) = self.current_spec() else {
            return;
        };
        let Some(dash) = self.dashboard_mut() else {
            return;
        };
        let sections = spec.sections.len();
        if sections == 0 {
            dash.slide = SlideRef { section: 0, tab: 0 };
            return;
        }
        let section = dash.slide.section.min(sections - 1);
        let tabs = spec.sections[section].tabs.len().max(1);
        dash.slide = SlideRef {
            section,
            tab: dash.slide.tab.min(tabs - 1),
        };
    }

    pub fn show_slide(&mut self, slide: SlideRef, cx: &mut Context<Self>) {
        if let Some(dash) = self.dashboard_mut() {
            dash.slide = slide;
            dash.title_inputs = None;
        }
        self.clamp_slide();
        cx.notify();
    }

    // ── Parameters, filters, visibility ───────────────────────────────────

    /// A parameter's current value as text.
    pub(crate) fn param_text(&self, name: &str) -> Option<String> {
        self.doc
            .params()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    fn param_waits_for_apply(&self, name: &str) -> bool {
        self.doc
            .model()
            .parameters
            .iter()
            .find(|p| p.name == name)
            .is_some_and(|p| p.reactivity.waits_for_apply())
    }

    /// Whether a tile shows now. Check cells report a pass once their
    /// last run succeeded.
    pub fn tile_visible(&self, tile: &Tile) -> bool {
        let param = |name: &str| self.param_text(name);
        let test = |cell: &str| {
            let state = self.doc.cell_state(cell);
            match state.status {
                CellStatus::Fresh | CellStatus::Cached => Some(true),
                CellStatus::Failed => Some(false),
                _ => None,
            }
        };
        tile.is_visible(&VisibilityInputs {
            param: &param,
            test_passed: &test,
        })
    }

    /// The parameters the shown dashboard's filter bar offers.
    pub(in crate::notebook) fn dashboard_filter_names(&self) -> Vec<String> {
        match self.current_spec() {
            Some(spec) if self.present.dashboard.is_some() => filter_params(&spec, self),
            _ => Vec::new(),
        }
    }

    /// After parameter values in use changed: a shown dashboard filtering
    /// by one of them offers a refresh.
    pub(in crate::notebook) fn dashboard_filters_changed(&mut self, changed: &[String]) {
        let names = self.dashboard_filter_names();
        if changed.iter().any(|c| names.contains(c))
            && let Some(dash) = self.dashboard_mut()
        {
            dash.refresh_offer = true;
        }
    }

    /// Act as if `value` was typed into the filter bar: applied at once for
    /// parameters that react right away, kept as a draft for parameters
    /// that wait for Apply.
    #[cfg(test)]
    pub fn set_filter_value(&mut self, name: &str, value: &str, cx: &mut Context<Self>) {
        use super::params::widget::Edit;
        let edit = if value.trim().is_empty() {
            Edit::Clear
        } else {
            Edit::Text(value.to_string())
        };
        self.edit_param(name, edit, Some(&WidgetSurface::Dashboard), cx);
    }

    fn commit_param(&mut self, name: &str, value: &str, cx: &mut Context<Self>) {
        if self.param_text(name).as_deref() == Some(value) {
            return;
        }
        self.set_param(name, value, cx);
        if self.param_error(name).is_none()
            && let Some(dash) = self.dashboard_mut()
        {
            dash.refresh_offer = true;
        }
        cx.notify();
    }

    /// Whether a filter has a value waiting for Apply.
    pub fn has_pending_filters(&self) -> bool {
        self.dashboard_filter_names()
            .iter()
            .any(|name| self.has_param_draft(name) || self.has_param_typing(name))
    }

    /// Apply every drafted filter value, including what was just typed.
    pub fn apply_filters(&mut self, cx: &mut Context<Self>) {
        let names = self.dashboard_filter_names();
        for name in &names {
            self.take_param_typing(name, cx);
        }
        self.apply_param_drafts(&names, cx);
    }

    /// Follow a tile's drill-through, setting its parameter to `value`
    /// (or the drill's fixed value).
    pub fn drill(&mut self, tile_id: &str, value: Option<String>, cx: &mut Context<Self>) {
        let Some(spec) = self.current_spec() else {
            return;
        };
        let Some(drill) = spec.tile(tile_id).and_then(|t| t.drill.clone()) else {
            return;
        };
        let Some(target) = spec.drill_target(&drill) else {
            return;
        };
        if let (Some(param), Some(value)) = (drill.param.as_deref(), value.or(drill.value.clone()))
        {
            self.commit_param(param, &value, cx);
        }
        self.show_slide(target, cx);
        if self.present.surface == super::present::Surface::Presentation
            && let Some(index) = spec.slides().iter().position(|s| *s == target)
        {
            self.present.slide = index;
        }
    }

    /// The values a drill-through menu offers for tile `tile_id`.
    pub fn drill_choices(&self, tile_id: &str) -> Vec<String> {
        let Some(spec) = self.current_spec() else {
            return Vec::new();
        };
        let Some(tile) = spec.tile(tile_id) else {
            return Vec::new();
        };
        let Some(column) = tile.drill.as_ref().and_then(|d| d.column.clone()) else {
            return Vec::new();
        };
        self.outputs
            .get(&tile.cell)
            .map(|o| column_values(&o, &column, DRILL_CHOICES))
            .unwrap_or_default()
    }

    // ── Refresh ───────────────────────────────────────────────────────────

    /// The tiles one section tab shows now (every tile while editing).
    fn shown_tiles(&self, spec: &LayoutSpec, slide: SlideRef) -> Vec<Tile> {
        let editing = self.dashboard().is_some_and(|d| d.editing);
        spec.tab_tiles(slide)
            .iter()
            .filter(|t| editing || self.tile_visible(t))
            .cloned()
            .collect()
    }

    /// The section tabs on screen: the current one, or all of them in a
    /// whole-dashboard presentation.
    fn shown_slides(&self, spec: &LayoutSpec) -> Vec<SlideRef> {
        let Some(dash) = self.dashboard() else {
            return Vec::new();
        };
        if self.present.surface == super::present::Surface::Presentation
            && spec.presentation.slides == layout::SlideMode::Whole
        {
            spec.sections
                .iter()
                .enumerate()
                .flat_map(|(s, sec)| {
                    (0..sec.tabs.len()).map(move |t| SlideRef { section: s, tab: t })
                })
                .collect()
        } else {
            vec![dash.slide]
        }
    }

    /// What Refresh would run for the shown tiles: their remote cells that
    /// are not current, and the connections those run on.
    pub fn refresh_plan(&self) -> RefreshPlan {
        let Some(spec) = self.current_spec() else {
            return RefreshPlan::default();
        };
        let tiles: Vec<Tile> = self
            .shown_slides(&spec)
            .into_iter()
            .flat_map(|slide| self.shown_tiles(&spec, slide))
            .collect();
        let ids: Vec<&str> = tiles.iter().map(|t| t.cell.as_str()).collect();
        let cells: Vec<String> = self
            .doc
            .remote_cells_behind(&ids)
            .into_iter()
            .filter(|id| {
                let state = self.doc.cell_state(id);
                !state.is_presentable_as_current() && !state.status.is_active()
            })
            .collect();
        let mut connections: Vec<(String, bool)> = Vec::new();
        for id in &cells {
            let (choice, _) = self.cell_connection(id);
            let (label, production) = match choice {
                Some(c) => (c.label, c.environment == ConnectionEnvironment::Production),
                None => (t("notebook.shell.connection.none"), false),
            };
            if !connections.iter().any(|(l, _)| *l == label) {
                connections.push((label, production));
            }
        }
        RefreshPlan { cells, connections }
    }

    /// Run the queries behind the shown tiles that are out of date.
    pub fn refresh_dashboard(&mut self, cx: &mut Context<Self>) {
        let plan = self.refresh_plan();
        if let Some(dash) = self.dashboard_mut() {
            dash.refresh_offer = false;
        }
        if plan.cells.is_empty() {
            cx.notify();
            return;
        }
        let selected = std::mem::replace(&mut self.selected, plan.cells);
        self.run_scope(RunScope::Selected, None, cx);
        self.selected = selected;
    }

    // ── Editing ───────────────────────────────────────────────────────────

    /// Add a tile for `cell` to the current tab (before `before` when
    /// dropped on a tile).
    pub fn add_tile(&mut self, cell: &str, before: Option<&str>, cx: &mut Context<Self>) {
        let Some(slide) = self.dashboard().map(|d| d.slide) else {
            return;
        };
        let cell = cell.to_string();
        let before = before.map(str::to_string);
        let mut added = None;
        self.edit_layout(
            |spec| {
                let id = spec.add_tile(slide.section, slide.tab, &cell);
                if let Some(before) = before {
                    place_before(spec, &id, &before);
                }
                added = Some(id);
                true
            },
            cx,
        );
        if let (Some(id), Some(dash)) = (added, self.dashboard_mut()) {
            dash.selected_tile = Some(id);
            dash.tile_inputs = None;
        }
        self.clamp_slide();
    }

    /// Drop a dragged tile before `before` (or at the end of the tab).
    pub fn drop_tile(&mut self, tile: &str, before: Option<&str>, cx: &mut Context<Self>) {
        let Some(slide) = self.dashboard().map(|d| d.slide) else {
            return;
        };
        let tile = tile.to_string();
        let before = before.map(str::to_string);
        self.edit_layout(
            |spec| {
                let mut changed = spec.move_tile_to(&tile, slide);
                match before {
                    Some(before) if before != tile => changed |= place_before(spec, &tile, &before),
                    Some(_) => {}
                    None => {
                        let pos = spec.tile_pos(&tile);
                        if let Some(pos) = pos {
                            let last = spec.sections[pos.section].tabs[pos.tab].tiles.len() - 1;
                            changed |= spec.move_tile(&tile, last as isize - pos.tile as isize);
                        }
                    }
                }
                changed
            },
            cx,
        );
    }

    pub fn remove_tile(&mut self, tile: &str, cx: &mut Context<Self>) {
        let tile = tile.to_string();
        if self.edit_layout(|spec| spec.remove_tile(&tile), cx)
            && let Some(dash) = self.dashboard_mut()
            && dash.selected_tile.as_deref() == Some(tile.as_str())
        {
            dash.selected_tile = None;
            dash.tile_inputs = None;
        }
    }

    pub fn move_tile(&mut self, tile: &str, delta: isize, cx: &mut Context<Self>) {
        let tile = tile.to_string();
        self.edit_layout(|spec| spec.move_tile(&tile, delta), cx);
    }

    pub fn select_tile(&mut self, tile: Option<String>, cx: &mut Context<Self>) {
        if let Some(dash) = self.dashboard_mut()
            && dash.selected_tile != tile
        {
            dash.selected_tile = tile;
            dash.tile_inputs = None;
        }
        cx.notify();
    }

    pub fn add_dashboard_section(&mut self, cx: &mut Context<Self>) {
        let mut slide = None;
        self.edit_layout(
            |spec| {
                let (section, tab) = spec.add_section();
                slide = Some(SlideRef { section, tab });
                true
            },
            cx,
        );
        if let Some(slide) = slide {
            self.show_slide(slide, cx);
        }
    }

    pub fn add_dashboard_tab(&mut self, cx: &mut Context<Self>) {
        let Some(section) = self.dashboard().map(|d| d.slide.section) else {
            return;
        };
        let mut slide = None;
        self.edit_layout(
            |spec| {
                if spec.sections.is_empty() {
                    let (section, tab) = spec.add_section();
                    slide = Some(SlideRef { section, tab });
                    return true;
                }
                let tab = spec.add_tab(section);
                slide = tab.map(|tab| SlideRef { section, tab });
                tab.is_some()
            },
            cx,
        );
        if let Some(slide) = slide {
            self.show_slide(slide, cx);
        }
    }

    pub fn remove_dashboard_tab(&mut self, cx: &mut Context<Self>) {
        let Some(slide) = self.dashboard().map(|d| d.slide) else {
            return;
        };
        let removed = self.edit_layout(
            |spec| {
                if spec
                    .sections
                    .get(slide.section)
                    .is_some_and(|s| s.tabs.len() > 1)
                {
                    spec.remove_tab(slide.section, slide.tab)
                } else {
                    spec.remove_section(slide.section)
                }
            },
            cx,
        );
        if removed {
            self.show_slide(
                SlideRef {
                    section: slide.section,
                    tab: slide.tab.saturating_sub(1),
                },
                cx,
            );
        }
    }

    pub fn set_filter_params(&mut self, params: Vec<String>, cx: &mut Context<Self>) {
        self.edit_layout(
            |spec| {
                let changed = spec.filters.params != params;
                spec.filters.params = params;
                changed
            },
            cx,
        );
    }

    pub fn set_tile_visibility(
        &mut self,
        tile: &str,
        rule: Option<Visibility>,
        cx: &mut Context<Self>,
    ) {
        let tile = tile.to_string();
        self.edit_layout(
            |spec| {
                let Some(t) = spec.tile_mut(&tile) else {
                    return false;
                };
                let next = rule.map(Condition::Known);
                let changed = t.visible_when != next;
                t.visible_when = next;
                changed
            },
            cx,
        );
    }

    pub fn set_tile_drill(&mut self, tile: &str, drill: Option<Drill>, cx: &mut Context<Self>) {
        let tile = tile.to_string();
        self.edit_layout(
            |spec| {
                let Some(t) = spec.tile_mut(&tile) else {
                    return false;
                };
                let changed = t.drill != drill;
                t.drill = drill;
                changed
            },
            cx,
        );
    }

    fn set_tile_text(&mut self, tile: &str, title: bool, value: &str, cx: &mut Context<Self>) {
        let tile = tile.to_string();
        let value = (!value.trim().is_empty()).then(|| value.to_string());
        self.edit_layout(
            |spec| {
                let Some(t) = spec.tile_mut(&tile) else {
                    return false;
                };
                let slot = if title {
                    &mut t.title
                } else {
                    &mut t.annotation
                };
                let changed = *slot != value;
                *slot = value;
                changed
            },
            cx,
        );
    }

    fn set_slide_title(&mut self, slide: SlideRef, tab: bool, value: &str, cx: &mut Context<Self>) {
        let value = value.trim().to_string();
        self.edit_layout(
            |spec| {
                let Some(section) = spec.sections.get_mut(slide.section) else {
                    return false;
                };
                let slot = if tab {
                    match section.tabs.get_mut(slide.tab) {
                        Some(t) => &mut t.title,
                        None => return false,
                    }
                } else {
                    &mut section.title
                };
                let changed = *slot != value;
                *slot = value;
                changed
            },
            cx,
        );
    }

    pub fn set_dashboard_slides(&mut self, mode: layout::SlideMode, cx: &mut Context<Self>) {
        self.edit_layout(
            |spec| {
                let changed = spec.presentation.slides != mode;
                spec.presentation.slides = mode;
                changed
            },
            cx,
        );
    }

    pub fn set_dashboard_theme(&mut self, theme: layout::ThemeChoice, cx: &mut Context<Self>) {
        self.edit_layout(
            |spec| {
                let changed = spec.theme != theme;
                spec.theme = theme;
                changed
            },
            cx,
        );
    }

    /// Ask for an image and use it as the dashboard's logo, after checking
    /// it is a clean PNG or SVG.
    pub fn pick_dashboard_logo(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let extension = path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let read = std::fs::read(&path);
            this.update(cx, |this, cx| {
                let logo = read
                    .map_err(|_| layout::LogoError::Unsupported)
                    .and_then(|bytes| layout::Logo::from_file(&extension, &bytes, ""));
                match logo {
                    Ok(logo) => {
                        this.edit_layout(
                            |spec| {
                                spec.logo = Some(logo);
                                true
                            },
                            cx,
                        );
                    }
                    Err(error) => {
                        this.status(crate::localization::message(error.message()), true, cx)
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn remove_dashboard_logo(&mut self, cx: &mut Context<Self>) {
        self.edit_layout(|spec| spec.logo.take().is_some(), cx);
    }

    pub fn begin_tile_resize(&mut self, tile: &str, width: bool, position: f32) {
        let Some(spec) = self.current_spec() else {
            return;
        };
        let Some(t) = spec.tile(tile) else {
            return;
        };
        let Some(dash) = self.dashboard_mut() else {
            return;
        };
        let breakpoint = dash.breakpoint();
        let (start_value, unit) = if width {
            (
                t.span(breakpoint),
                (dash.width.get() / layout::GRID_COLUMNS as f32).max(1.0),
            )
        } else {
            (t.height(), Theme::scaled(ROW_HEIGHT))
        };
        dash.resize = Some(TileResize {
            tile: tile.to_string(),
            width,
            start: position,
            start_value,
            unit,
        });
    }

    /// Follow the pointer while a tile edge is dragged.
    pub fn resize_tile_to(&mut self, position: f32, cx: &mut Context<Self>) {
        let Some((resize, breakpoint)) = self
            .dashboard()
            .and_then(|d| d.resize.clone().map(|r| (r, d.breakpoint())))
        else {
            return;
        };
        let steps = ((position - resize.start) / resize.unit).round() as i32;
        let value = (resize.start_value as i32 + steps).clamp(1, 12) as u8;
        let tile = resize.tile.clone();
        self.edit_layout(
            |spec| {
                if resize.width {
                    spec.set_span(&tile, breakpoint, value)
                } else {
                    spec.set_height(&tile, value)
                }
            },
            cx,
        );
    }

    pub fn end_tile_resize(&mut self, cx: &mut Context<Self>) {
        if let Some(dash) = self.dashboard_mut() {
            dash.resize = None;
        }
        cx.notify();
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    fn ensure_dashboard_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spec) = self.current_spec() else {
            return;
        };
        let Some(dash) = self.present.dashboard.as_ref() else {
            return;
        };
        let editing = dash.editing;
        let slide = dash.slide;
        let selected = dash.selected_tile.clone();
        self.ensure_param_widgets(window, cx);
        if !editing {
            return;
        }
        let tile_input_for = self
            .dashboard()
            .and_then(|d| d.tile_inputs.as_ref().map(|(id, _, _)| id.clone()));
        if let Some(tile_id) = selected.filter(|id| tile_input_for.as_deref() != Some(id.as_str()))
            && let Some(tile) = spec.tile(&tile_id)
        {
            let title = self.text_input(
                window,
                cx,
                "notebook.dashboard.settings.title_placeholder",
                tile.title.as_deref().unwrap_or(""),
            );
            let note = self.text_input(
                window,
                cx,
                "notebook.dashboard.settings.annotation_placeholder",
                tile.annotation.as_deref().unwrap_or(""),
            );
            for (input, is_title) in [(&title, true), (&note, false)] {
                let id = tile_id.clone();
                cx.subscribe(input, move |this, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        this.set_tile_text(&id, is_title, &value, cx);
                    }
                })
                .detach();
            }
            if let Some(dash) = self.dashboard_mut() {
                dash.tile_inputs = Some((tile_id, title, note));
            }
        }
        let titles_for = self
            .dashboard()
            .and_then(|d| d.title_inputs.as_ref().map(|(s, _, _)| *s));
        if titles_for != Some(slide)
            && let Some(section) = spec.sections.get(slide.section)
        {
            let tab_title = section
                .tabs
                .get(slide.tab)
                .map(|t| t.title.clone())
                .unwrap_or_default();
            let section_input = self.text_input(
                window,
                cx,
                "notebook.dashboard.section.title_placeholder",
                &section.title,
            );
            let tab_input = self.text_input(
                window,
                cx,
                "notebook.dashboard.tab.title_placeholder",
                &tab_title,
            );
            for (input, is_tab) in [(&section_input, false), (&tab_input, true)] {
                cx.subscribe(input, move |this, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        this.set_slide_title(slide, is_tab, &value, cx);
                    }
                })
                .detach();
            }
            if let Some(dash) = self.dashboard_mut() {
                dash.title_inputs = Some((slide, section_input, tab_input));
            }
        }
    }

    fn text_input(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        placeholder: &'static str,
        value: &str,
    ) -> Entity<InputState> {
        let value = value.to_string();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                placeholder,
            ))
        });
        if !value.is_empty() {
            input.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        input
    }

    /// The dashboard editor or viewer, filling the notebook tab.
    pub(super) fn render_dashboard_surface(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_dashboard_inputs(window, cx);
        let Some(dash) = self.dashboard() else {
            return div().into_any_element();
        };
        let editing = dash.editing;
        let slide = dash.slide;
        let cell = dash.cell.clone();
        let spec = match self.layout_spec(&cell) {
            Ok(spec) => spec,
            Err(message) => return unreadable(message, cx),
        };
        div()
            .id("nb-dashboard")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(Theme::bg())
            .child(self.render_dashboard_header(&spec, editing, cx))
            .child(self.render_section_strip(&spec, editing, cx))
            .child(self.render_filter_bar(&spec, editing, cx))
            .children(self.render_refresh_offer(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("nb-dashboard-scroll")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .child(self.render_canvas(&spec, slide, editing, window, cx)),
                    )
                    .when(editing, |d| d.child(self.render_editor_panel(&spec, cx))),
            )
            .into_any_element()
    }

    fn render_dashboard_header(
        &mut self,
        spec: &LayoutSpec,
        editing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let title = if spec.title.trim().is_empty() {
            self.doc
                .title()
                .map(str::to_string)
                .unwrap_or_else(|| t("notebook.dashboard.header.untitled"))
        } else {
            spec.title.clone()
        };
        let plan = self.refresh_plan();
        let refresh_tooltip = refresh_text(&plan);
        let issues = spec.validate(self.doc.model()).len();
        let logo = spec.logo.as_ref().and_then(logo_image);
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(5.0)))
            .border_b_1()
            .border_color(Theme::border())
            .bg(Theme::tab_strip_bg())
            .when(editing, |d| {
                d.child(
                    text_button("nb-dash-back", t("notebook.dashboard.header.back"))
                        .icon(Icon::empty().path(AppIcon::ChevronLeft.path()))
                        .on_click(cx.listener(|this, _, _, cx| this.close_dashboard(cx))),
                )
            })
            .when_some(logo, |d, logo| {
                d.child(gpui::img(logo).h(px(s(22.0))).max_w(px(s(120.0))))
            })
            .child(
                div()
                    .text_color(Theme::text())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(t(if editing {
                        "notebook.dashboard.header.mode_editor"
                    } else {
                        "notebook.dashboard.header.mode_viewer"
                    })),
            )
            .when(editing && issues > 0, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::warning())
                        .child(crate::localization::format(
                            if issues == 1 {
                                "notebook.dashboard.header.issue_count_one"
                            } else {
                                "notebook.dashboard.header.issue_count"
                            },
                            &[("count", issues)],
                        )),
                )
            })
            .child(div().flex_1())
            .when(editing, |d| {
                d.child(
                    text_button("nb-dash-add-tile", t("notebook.dashboard.header.add_tile"))
                        .icon(Icon::empty().path(AppIcon::Add.path()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(dash) = this.dashboard_mut() {
                                dash.picker = !dash.picker;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    text_button(
                        "nb-dash-add-section",
                        t("notebook.dashboard.header.add_section"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.add_dashboard_section(cx))),
                )
                .child(
                    text_button("nb-dash-add-tab", t("notebook.dashboard.header.add_tab"))
                        .on_click(cx.listener(|this, _, _, cx| this.add_dashboard_tab(cx))),
                )
            })
            .child(
                text_button("nb-dash-refresh", t("notebook.dashboard.refresh.button"))
                    .icon(Icon::empty().path(AppIcon::RefreshSchema.path()))
                    .tooltip(refresh_tooltip)
                    .disabled(plan.cells.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_dashboard(cx))),
            )
            .child(
                text_button("nb-dash-export", t("notebook.dashboard.header.export")).on_click(
                    cx.listener(|this, _, window, cx| {
                        let scope = this.dashboard().map(|d| {
                            crate::notebook::export_dialog::ExportScope::Dashboard(d.cell.clone())
                        });
                        this.open_export_dialog(None, scope, window, cx)
                    }),
                ),
            )
            .child(if editing {
                text_button("nb-dash-view", t("notebook.dashboard.header.view"))
                    .icon(Icon::empty().path(AppIcon::View.path()))
                    .on_click(cx.listener(|this, _, _, cx| this.set_dashboard_editing(false, cx)))
            } else {
                text_button("nb-dash-edit", t("notebook.dashboard.header.edit"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_dashboard_editing(true, cx)))
            })
            .child(
                text_button("nb-dash-present", t("notebook.dashboard.header.present"))
                    .icon(Icon::empty().path(AppIcon::Play.path()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let cell = this.dashboard().map(|d| d.cell.clone());
                        this.start_presentation(cell, window, cx)
                    })),
            )
            .when(!editing, |d| {
                d.child(
                    icon_button(
                        "nb-dash-close",
                        AppIcon::Close,
                        t("notebook.dashboard.header.close_tooltip"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.close_dashboard(cx))),
                )
            })
            .into_any_element()
    }

    fn render_section_strip(
        &mut self,
        spec: &LayoutSpec,
        editing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let current = self
            .dashboard()
            .map(|d| d.slide)
            .unwrap_or(SlideRef { section: 0, tab: 0 });
        let mut sections = div().flex().flex_row().flex_wrap().gap(px(s(4.0)));
        for (n, section) in spec.sections.iter().enumerate() {
            let label = section_label(&section.title, n);
            let slide = SlideRef { section: n, tab: 0 };
            sections = sections.child(
                text_button(format!("nb-dash-section-{n}"), label)
                    .selected(current.section == n)
                    .on_click(cx.listener(move |this, _, _, cx| this.show_slide(slide, cx))),
            );
        }
        let mut tabs = div().flex().flex_row().flex_wrap().gap(px(s(4.0)));
        let tab_count = spec
            .sections
            .get(current.section)
            .map(|sec| sec.tabs.len())
            .unwrap_or(0);
        if tab_count > 1 || editing {
            for (n, tab) in spec
                .sections
                .get(current.section)
                .map(|sec| sec.tabs.as_slice())
                .unwrap_or(&[])
                .iter()
                .enumerate()
            {
                let slide = SlideRef {
                    section: current.section,
                    tab: n,
                };
                tabs = tabs.child(
                    text_button(format!("nb-dash-tab-{n}"), tab_label(&tab.title, n))
                        .selected(current.tab == n)
                        .on_click(cx.listener(move |this, _, _, cx| this.show_slide(slide, cx))),
                );
            }
        }
        let title_inputs = self
            .dashboard()
            .and_then(|d| d.title_inputs.as_ref())
            .filter(|(slide, _, _)| *slide == current)
            .map(|(_, a, b)| (a.clone(), b.clone()));
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(10.0)))
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(sections)
            .when(tab_count > 1 || editing, |d| {
                d.child(div().w(px(1.0)).h(px(s(16.0))).bg(Theme::border()))
                    .child(tabs)
            })
            .when(editing, |d| {
                d.when_some(title_inputs, |d, (section, tab)| {
                    d.child(div().w(px(s(160.0))).child(Input::new(&section).small()))
                        .child(div().w(px(s(140.0))).child(Input::new(&tab).small()))
                })
                .when(!spec.sections.is_empty(), |d| {
                    d.child(
                        icon_button(
                            "nb-dash-remove-tab",
                            AppIcon::Minus,
                            t(if tab_count > 1 {
                                "notebook.dashboard.tab.remove"
                            } else {
                                "notebook.dashboard.section.remove"
                            }),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.remove_dashboard_tab(cx))),
                    )
                })
            })
            .into_any_element()
    }

    pub(super) fn render_filter_bar(
        &mut self,
        spec: &LayoutSpec,
        editing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let widgets: Vec<Entity<ParamWidget>> = filter_params(spec, self)
            .iter()
            .filter_map(|name| {
                self.param_ui
                    .widget(&WidgetSurface::Dashboard, name)
                    .cloned()
            })
            .collect();
        let pending = self.has_pending_filters();
        let any_apply = filter_params(spec, self)
            .iter()
            .any(|n| self.param_waits_for_apply(n));
        let mut bar = div()
            .id("nb-dash-filters")
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(10.0)))
            .py(px(s(5.0)))
            .border_b_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg());
        if widgets.is_empty() {
            bar = bar.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(t(if editing {
                        "notebook.dashboard.filters.none_editor"
                    } else {
                        "notebook.dashboard.filters.none"
                    })),
            );
        }
        for widget in widgets {
            bar = bar.child(div().w(px(s(220.0))).child(widget));
        }
        if any_apply {
            bar = bar
                .child(
                    text_button("nb-dash-apply", t("notebook.dashboard.filters.apply"))
                        .disabled(!pending)
                        .on_click(cx.listener(|this, _, _, cx| this.apply_filters(cx))),
                )
                .when(pending, |d| {
                    d.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::warning())
                            .child(t("notebook.dashboard.filters.pending")),
                    )
                });
        }
        if editing {
            let all: Vec<String> = self
                .doc
                .model()
                .parameters
                .iter()
                .map(|p| p.name.clone())
                .collect();
            let chosen = spec.filters.params.clone();
            let this = cx.entity();
            bar = bar.child(
                text_button(
                    "nb-dash-filter-menu",
                    t("notebook.dashboard.filters.choose"),
                )
                .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                .disabled(all.is_empty())
                .dropdown_menu(move |mut menu, _, _| {
                    for name in &all {
                        let on = chosen.contains(name);
                        let this = this.clone();
                        let mut next = chosen.clone();
                        if on {
                            next.retain(|n| n != name);
                        } else {
                            next.push(name.clone());
                        }
                        menu = menu.item(PopupMenuItem::new(name.clone()).checked(on).on_click(
                            move |_, _, app| {
                                let next = next.clone();
                                this.update(app, |v, cx| v.set_filter_params(next, cx));
                            },
                        ));
                    }
                    menu
                }),
            );
        }
        bar.into_any_element()
    }

    fn render_refresh_offer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let offer = self.dashboard()?.refresh_offer;
        let plan = self.refresh_plan();
        if !offer || plan.cells.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        let production = plan.connections.iter().any(|(_, p)| *p);
        Some(
            div()
                .id("nb-dash-refresh-offer")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .px(px(s(10.0)))
                .py(px(s(5.0)))
                .border_b_1()
                .border_color(if production {
                    Theme::error()
                } else {
                    Theme::warning()
                })
                .bg(Theme::panel_bg())
                .child(
                    div()
                        .flex_1()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(refresh_text(&plan)),
                )
                .children(plan.connections.iter().filter(|(_, p)| *p).map(|_| {
                    crate::notebook::connection::environment_badge(
                        ConnectionEnvironment::Production,
                    )
                    .into_any_element()
                }))
                .child(
                    text_button(
                        "nb-dash-refresh-now",
                        t("notebook.dashboard.refresh.offer_button"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_dashboard(cx))),
                )
                .child(
                    icon_button(
                        "nb-dash-refresh-dismiss",
                        AppIcon::Close,
                        t("notebook.dashboard.refresh.dismiss"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(dash) = this.dashboard_mut() {
                            dash.refresh_offer = false;
                        }
                        cx.notify();
                    })),
                )
                .into_any_element(),
        )
    }

    /// One section tab of the dashboard as grid rows.
    pub(super) fn render_canvas(
        &mut self,
        spec: &LayoutSpec,
        slide: SlideRef,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let Some(dash) = self.dashboard() else {
            return div().into_any_element();
        };
        let breakpoint = dash.breakpoint();
        let width_cell = dash.width.clone();
        let entity = cx.entity_id();
        let tiles = self.shown_tiles(spec, slide);
        let refs: Vec<&Tile> = tiles.iter().collect();
        let rows = layout::pack_rows(&refs, breakpoint);
        let measure = canvas(
            move |bounds, _, cx| {
                let width = f32::from(bounds.size.width);
                if (width_cell.get() - width).abs() > 0.5 {
                    width_cell.set(width);
                    cx.notify(entity);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let mut grid = div()
            .id("nb-dash-canvas")
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .p(px(s(TILE_GUTTER)))
            .child(measure);
        if spec.sections.is_empty() || tiles.is_empty() {
            grid = grid.child(div().p(px(s(24.0))).text_color(Theme::text_dim()).child(t(
                if editing {
                    "notebook.dashboard.canvas.empty_editor"
                } else {
                    "notebook.dashboard.canvas.empty"
                },
            )));
        }
        for row in rows {
            let mut line = div().flex().flex_row().w_full();
            for index in row {
                let tile = &tiles[index];
                line = line.child(self.render_tile(tile, breakpoint, editing, window, cx));
            }
            grid = grid.child(line);
        }
        if editing {
            grid = grid
                .drag_over::<DraggedTile>(|style, _, _, _| style.bg(Theme::row_hover()))
                .on_drop(
                    cx.listener(|this, dragged: &DraggedTile, _, cx| match dragged {
                        DraggedTile::Cell(cell) => this.add_tile(cell, None, cx),
                        DraggedTile::Tile(tile) => this.drop_tile(tile, None, cx),
                    }),
                );
        }
        grid.into_any_element()
    }

    fn render_tile(
        &mut self,
        tile: &Tile,
        breakpoint: Breakpoint,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let span = tile.span(breakpoint);
        let height = Theme::scaled(ROW_HEIGHT) * tile.height() as f32;
        let cell = self.doc.cell_by_id(&tile.cell);
        let kind = cell.map(|c| c.kind.to_string());
        let cell_name = cell.map(|c| c.display_name().to_string());
        let title = tile
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                cell.filter(|c| c.kind != KIND_MARKDOWN)
                    .map(|c| c.display_name().to_string())
            });
        let state = self.doc.status(&tile.cell);
        let stale = self.is_stale(&tile.cell);
        let hidden = editing && !self.tile_visible(tile);
        let selected = editing
            && self
                .dashboard()
                .is_some_and(|d| d.selected_tile.as_deref() == Some(tile.id.as_str()));
        let body = self.render_tile_body(tile, kind.as_deref(), window, cx);
        let drill = (!editing)
            .then(|| tile.drill.clone())
            .flatten()
            .map(|d| self.render_drill_button(tile, &d, cx));
        let tile_id = tile.id.clone();
        let pos = SharedString::from(format!("nb-tile-{}", tile.id));
        let mut frame =
            div()
                .id(pos)
                .relative()
                .flex()
                .flex_col()
                .h(px(height))
                .rounded(px(s(5.0)))
                .border_1()
                .border_color(if selected {
                    Theme::accent()
                } else {
                    Theme::border()
                })
                .bg(Theme::panel_bg())
                .overflow_hidden()
                .when(hidden, |d| d.opacity(0.55))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .px(px(s(8.0)))
                        .min_h(px(Theme::row_height()))
                        .when_some(title, |d, title| {
                            d.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(Theme::text())
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                        })
                        .when(stale && !editing, |d| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.8)))
                                    .text_color(Theme::warning())
                                    .child(t("notebook.dashboard.tile.stale")),
                            )
                        })
                        .when(state.is_active(), |d| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.8)))
                                    .text_color(Theme::text_dim())
                                    .child(t("notebook.dashboard.tile.running")),
                            )
                        })
                        .when(hidden, |d| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.8)))
                                    .text_color(Theme::text_dim())
                                    .child(t("notebook.dashboard.tile.hidden")),
                            )
                        })
                        .children(drill)
                        .when(editing, |d| {
                            let left = tile_id.clone();
                            let right = tile_id.clone();
                            let remove = tile_id.clone();
                            d.child(div().flex_1())
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::format(
                                            "notebook.dashboard.tile.span_label",
                                            &[("span", span)],
                                        )),
                                )
                                .child(
                                    icon_button(
                                        format!("nb-tile-left-{tile_id}"),
                                        AppIcon::ChevronLeft,
                                        t("notebook.dashboard.tile.move_left"),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.move_tile(&left, -1, cx),
                                    )),
                                )
                                .child(
                                    icon_button(
                                        format!("nb-tile-right-{tile_id}"),
                                        AppIcon::ChevronRight,
                                        t("notebook.dashboard.tile.move_right"),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.move_tile(&right, 1, cx),
                                    )),
                                )
                                .child(
                                    icon_button(
                                        format!("nb-tile-remove-{tile_id}"),
                                        AppIcon::Close,
                                        t("notebook.dashboard.tile.remove"),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.remove_tile(&remove, cx),
                                    )),
                                )
                        }),
                )
                .child(div().flex_1().min_h_0().w_full().child(body))
                .when_some(
                    tile.annotation.clone().filter(|a| !a.trim().is_empty()),
                    |d, note| {
                        d.child(
                            div()
                                .px(px(s(8.0)))
                                .py(px(s(4.0)))
                                .border_t_1()
                                .border_color(Theme::border())
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(crate::notebook::cells::markdown::markdown_view(
                                    SharedString::from(format!("nb-tile-note-{}", tile.id)),
                                    SharedString::from(note),
                                    None,
                                )),
                        )
                    },
                );
        if editing {
            let select = tile_id.clone();
            let width_handle = tile_id.clone();
            let height_handle = tile_id.clone();
            let label = title_or_id(tile, cell_name.clone());
            let drop_before = tile_id.clone();
            frame = frame
                .cursor(CursorStyle::OpenHand)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_tile(Some(select.clone()), cx)),
                )
                .on_drag(DraggedTile::Tile(tile_id.clone()), move |_, _, _, cx| {
                    let label = label.clone();
                    cx.new(|_| DragLabel(label))
                })
                .drag_over::<DraggedTile>(|style, _, _, _| style.border_color(Theme::accent()))
                .on_drop(cx.listener(move |this, dragged: &DraggedTile, _, cx| {
                    match dragged {
                        DraggedTile::Cell(cell) => this.add_tile(cell, Some(&drop_before), cx),
                        DraggedTile::Tile(tile) => this.drop_tile(tile, Some(&drop_before), cx),
                    }
                    cx.stop_propagation();
                }))
                .child(
                    div()
                        .id(SharedString::from(format!("nb-tile-w-{tile_id}")))
                        .absolute()
                        .top_0()
                        .right_0()
                        .h_full()
                        .w(px(s(6.0)))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .hover(|style| style.bg(Theme::accent()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                this.begin_tile_resize(
                                    &width_handle,
                                    true,
                                    f32::from(event.position.x),
                                );
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("nb-tile-h-{tile_id}")))
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .h(px(s(6.0)))
                        .cursor(CursorStyle::ResizeUpDown)
                        .hover(|style| style.bg(Theme::accent()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                this.begin_tile_resize(
                                    &height_handle,
                                    false,
                                    f32::from(event.position.y),
                                );
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                );
        }
        div()
            .w(relative(span as f32 / layout::GRID_COLUMNS as f32))
            .p(px(s(TILE_GUTTER)))
            .child(frame)
            .into_any_element()
    }

    fn render_tile_body(
        &mut self,
        tile: &Tile,
        kind: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let note = |text: String| {
            div()
                .p(px(s(10.0)))
                .text_size(px(Theme::font_size_ratio(0.9)))
                .text_color(Theme::text_dim())
                .child(text)
                .into_any_element()
        };
        match tile_body(kind) {
            TileBody::Missing => note(t("notebook.dashboard.tile.missing_cell")),
            TileBody::Text => {
                let source = self.interpolate_markdown(&tile.cell).unwrap_or_else(|| {
                    self.doc
                        .cell_by_id(&tile.cell)
                        .map(|c| c.source.to_string())
                        .unwrap_or_default()
                });
                let images = self.markdown_images(&source, cx);
                div()
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .child(crate::notebook::cells::markdown::markdown_view(
                        SharedString::from(format!("nb-tile-md-{}", tile.id)),
                        SharedString::from(source),
                        images,
                    ))
                    .into_any_element()
            }
            TileBody::Grid => match self.tile_grid(&tile.cell, cx) {
                Some(grid) => div().size_full().child(grid).into_any_element(),
                None => {
                    let message = self.cells.get(&tile.cell).and_then(|rt| rt.message_text());
                    note(message.unwrap_or_else(|| t("notebook.dashboard.tile.no_output")))
                }
            },
            TileBody::Output => {
                let Some(render_cx) = self.output_render_cx(&tile.cell, window, cx) else {
                    return note(t("notebook.dashboard.tile.missing_cell"));
                };
                cells::renderer_for(&render_cx.kind).render_output(&render_cx, window, cx)
            }
        }
    }

    /// A read-only grid of a SQL cell's last result, rebuilt when the cell
    /// produces a new result.
    fn tile_grid(&mut self, cell: &str, cx: &mut Context<Self>) -> Option<Entity<SqllyDataTable>> {
        let version = self.outputs.version(cell);
        if let Some((seen, grid)) = self.dashboard().and_then(|d| d.grids.get(cell))
            && *seen == version
        {
            return Some(grid.clone());
        }
        let output = self.outputs.get(cell)?;
        let set = crate::tool_dialog::ToolQuerySet {
            columns: output.columns.clone(),
            rows: output.rows.as_ref().clone(),
        };
        let grid = crate::tool_dialog::build_result_grid(&set, cx)?;
        if let Some(dash) = self.dashboard_mut() {
            dash.grids.insert(cell.to_string(), (version, grid.clone()));
        }
        Some(grid)
    }

    fn render_drill_button(
        &mut self,
        tile: &Tile,
        drill: &Drill,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tile_id = tile.id.clone();
        let label = t("notebook.dashboard.tile.drill");
        if drill.column.is_none() {
            return text_button(format!("nb-tile-drill-{tile_id}"), label)
                .icon(Icon::empty().path(AppIcon::ChevronRight.path()))
                .on_click(cx.listener(move |this, _, _, cx| this.drill(&tile_id, None, cx)))
                .into_any_element();
        }
        let choices = self.drill_choices(&tile.id);
        let this = cx.entity();
        text_button(format!("nb-tile-drill-{tile_id}"), label)
            .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
            .disabled(choices.is_empty())
            .dropdown_menu(move |mut menu, _, _| {
                for value in &choices {
                    let this = this.clone();
                    let tile_id = tile_id.clone();
                    let picked = value.clone();
                    menu = menu.item(PopupMenuItem::new(value.clone()).on_click(
                        move |_, _, app| {
                            let picked = picked.clone();
                            this.update(app, |v, cx| v.drill(&tile_id, Some(picked), cx));
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }

    /// The editor's side panel: the Add tile picker, the selected tile's
    /// settings, the dashboard's settings and its problems.
    fn render_editor_panel(&mut self, spec: &LayoutSpec, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let picker = self.dashboard().is_some_and(|d| d.picker);
        let mut panel = div()
            .id("nb-dash-panel")
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .w(px(s(280.0)))
            .h_full()
            .p(px(s(10.0)))
            .border_l_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg())
            .overflow_y_scroll();
        if picker {
            panel = panel.child(self.render_tile_picker(cx));
        }
        if let Some(settings) = self.render_tile_settings(spec, cx) {
            panel = panel.child(settings);
        }
        panel = panel.child(self.render_dashboard_settings(spec, cx));
        let issues = spec.validate(self.doc.model());
        if !issues.is_empty() {
            panel = panel.child(heading(t("notebook.dashboard.settings.problems")));
            for issue in issues {
                panel = panel.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::warning())
                        .child(crate::localization::message(issue.message())),
                );
            }
        }
        panel.into_any_element()
    }

    fn render_tile_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let cells: Vec<(String, String, String)> = self
            .doc
            .model()
            .cells
            .iter()
            .filter(|c| layout::can_tile(&c.kind))
            .map(|c| {
                (
                    c.id.clone(),
                    c.relation_name().to_string(),
                    t(cells::renderer_for(c.kind.as_str()).label_key()),
                )
            })
            .collect();
        let mut list = div()
            .id("nb-dash-picker")
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .child(heading(t("notebook.dashboard.picker.title")))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(t("notebook.dashboard.picker.hint")),
            );
        if cells.is_empty() {
            list = list.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(t("notebook.dashboard.picker.empty")),
            );
        }
        for (id, name, kind) in cells {
            let add = id.clone();
            let label = name.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("nb-dash-pick-{id}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(6.0)))
                    .py(px(s(3.0)))
                    .rounded(px(s(3.0)))
                    .cursor(CursorStyle::OpenHand)
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.add_tile(&add, None, cx)))
                    .on_drag(DraggedTile::Cell(id.clone()), move |_, _, _, cx| {
                        let label = label.clone();
                        cx.new(|_| DragLabel(label))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_color(Theme::text())
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(kind),
                    ),
            );
        }
        list.into_any_element()
    }

    fn render_tile_settings(
        &mut self,
        spec: &LayoutSpec,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = Theme::scaled;
        let dash = self.dashboard()?;
        let tile = spec.tile(dash.selected_tile.as_deref()?)?.clone();
        let (title, note) = dash
            .tile_inputs
            .as_ref()
            .filter(|(id, _, _)| *id == tile.id)
            .map(|(_, a, b)| (a.clone(), b.clone()))?;
        let this = cx.entity();
        let params: Vec<String> = self
            .doc
            .model()
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let checks: Vec<(String, String)> = self
            .doc
            .model()
            .cells
            .iter()
            .filter(|c| c.kind == CellKind::Assertion)
            .map(|c| (c.id.clone(), c.relation_name().to_string()))
            .collect();
        let visibility_label = match &tile.visible_when {
            None => t("notebook.dashboard.settings.always"),
            Some(Condition::Known(Visibility::ParamHasValue { param })) => {
                crate::localization::format(
                    "notebook.dashboard.settings.when_param",
                    &[("param", param.as_str())],
                )
            }
            Some(Condition::Known(Visibility::ParamEquals { param, value })) => {
                crate::localization::format(
                    "notebook.dashboard.settings.when_param_equals",
                    &[("param", param.as_str()), ("value", value.as_str())],
                )
            }
            Some(Condition::Known(Visibility::TestPasses { cell })) => crate::localization::format(
                "notebook.dashboard.settings.when_check",
                &[("cell", cell.as_str())],
            ),
            Some(Condition::Other(_)) => t("notebook.dashboard.settings.unsupported_rule"),
        };
        let visibility_menu = {
            let this = this.clone();
            let tile_id = tile.id.clone();
            let params = params.clone();
            text_button("nb-dash-visibility", visibility_label)
                .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                .dropdown_menu(move |mut menu, _, _| {
                    let always = this.clone();
                    let id = tile_id.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t("notebook.dashboard.settings.always")).on_click(
                            move |_, _, app| {
                                always.update(app, |v, cx| v.set_tile_visibility(&id, None, cx))
                            },
                        ),
                    );
                    for param in &params {
                        let this = this.clone();
                        let id = tile_id.clone();
                        let rule = Visibility::ParamHasValue {
                            param: param.clone(),
                        };
                        menu = menu.item(
                            PopupMenuItem::new(crate::localization::format(
                                "notebook.dashboard.settings.when_param",
                                &[("param", param.as_str())],
                            ))
                            .on_click(move |_, _, app| {
                                let rule = rule.clone();
                                this.update(app, |v, cx| v.set_tile_visibility(&id, Some(rule), cx))
                            }),
                        );
                    }
                    for (cell, name) in &checks {
                        let this = this.clone();
                        let id = tile_id.clone();
                        let rule = Visibility::TestPasses { cell: cell.clone() };
                        menu = menu.item(
                            PopupMenuItem::new(crate::localization::format(
                                "notebook.dashboard.settings.when_check",
                                &[("cell", name.as_str())],
                            ))
                            .on_click(move |_, _, app| {
                                let rule = rule.clone();
                                this.update(app, |v, cx| v.set_tile_visibility(&id, Some(rule), cx))
                            }),
                        );
                    }
                    menu
                })
        };
        let drill_label = match &tile.drill {
            None => t("notebook.dashboard.settings.drill_none"),
            Some(drill) => {
                let section = spec
                    .drill_target(drill)
                    .map(|slide| section_label(&spec.sections[slide.section].title, slide.section))
                    .unwrap_or_else(|| drill.section.clone());
                match (&drill.param, &drill.column) {
                    (Some(param), Some(column)) => crate::localization::format(
                        "notebook.dashboard.settings.drill_param",
                        &[
                            ("section", section.as_str()),
                            ("param", param.as_str()),
                            ("column", column.as_str()),
                        ],
                    ),
                    _ => crate::localization::format(
                        "notebook.dashboard.settings.drill_to",
                        &[("section", section.as_str())],
                    ),
                }
            }
        };
        let columns: Vec<String> = self
            .outputs
            .get(&tile.cell)
            .map(|o| o.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        let targets: Vec<(String, String)> = spec
            .sections
            .iter()
            .enumerate()
            .map(|(n, sec)| (sec.id.clone(), section_label(&sec.title, n)))
            .collect();
        let drill_menu =
            {
                let this = this.clone();
                let tile_id = tile.id.clone();
                text_button("nb-dash-drill", drill_label)
                    .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                    .dropdown_menu(move |mut menu, _, _| {
                        let none = this.clone();
                        let id = tile_id.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t("notebook.dashboard.settings.drill_none"))
                                .on_click(move |_, _, app| {
                                    none.update(app, |v, cx| v.set_tile_drill(&id, None, cx))
                                }),
                        );
                        for (section, name) in &targets {
                            let this_plain = this.clone();
                            let id = tile_id.clone();
                            let plain = Drill {
                                section: section.clone(),
                                ..Drill::default()
                            };
                            menu = menu.item(
                                PopupMenuItem::new(crate::localization::format(
                                    "notebook.dashboard.settings.drill_to",
                                    &[("section", name.as_str())],
                                ))
                                .on_click(move |_, _, app| {
                                    let drill = plain.clone();
                                    this_plain
                                        .update(app, |v, cx| v.set_tile_drill(&id, Some(drill), cx))
                                }),
                            );
                            for param in &params {
                                for column in &columns {
                                    let this = this.clone();
                                    let id = tile_id.clone();
                                    let drill = Drill {
                                        section: section.clone(),
                                        param: Some(param.clone()),
                                        column: Some(column.clone()),
                                        ..Drill::default()
                                    };
                                    menu =
                                        menu.item(
                                            PopupMenuItem::new(crate::localization::format(
                                                "notebook.dashboard.settings.drill_param",
                                                &[
                                                    ("section", name.as_str()),
                                                    ("param", param.as_str()),
                                                    ("column", column.as_str()),
                                                ],
                                            ))
                                            .on_click(move |_, _, app| {
                                                let drill = drill.clone();
                                                this.update(app, |v, cx| {
                                                    v.set_tile_drill(&id, Some(drill), cx)
                                                })
                                            }),
                                        );
                                }
                            }
                        }
                        menu
                    })
            };
        let compact = tile.span.compact;
        let wide = tile.span.wide;
        let compact_id = tile.id.clone();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(heading(t("notebook.dashboard.settings.title")))
                .child(Input::new(&title).small())
                .child(Input::new(&note).small())
                .child(label(t("notebook.dashboard.settings.visibility")))
                .child(visibility_menu)
                .child(label(t("notebook.dashboard.settings.drill")))
                .child(drill_menu)
                .child(label(crate::localization::format(
                    "notebook.dashboard.settings.width",
                    &[("wide", wide), ("compact", compact)],
                )))
                .child(
                    text_button(
                        "nb-dash-compact-span",
                        t(if compact == layout::GRID_COLUMNS {
                            "notebook.dashboard.settings.compact_half"
                        } else {
                            "notebook.dashboard.settings.compact_full"
                        }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if compact == layout::GRID_COLUMNS {
                            6
                        } else {
                            12
                        };
                        let id = compact_id.clone();
                        this.edit_layout(|spec| spec.set_span(&id, Breakpoint::Compact, next), cx);
                    })),
                )
                .into_any_element(),
        )
    }

    fn render_dashboard_settings(
        &mut self,
        spec: &LayoutSpec,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let slides = spec.presentation.slides;
        let theme = spec.theme;
        let has_logo = spec.logo.is_some();
        let this = cx.entity();
        let theme_label = t(theme_label_key(theme));
        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(heading(t("notebook.dashboard.settings.dashboard")))
            .child(
                text_button(
                    "nb-dash-slides",
                    t(match slides {
                        layout::SlideMode::PerSection => {
                            "notebook.dashboard.settings.slides_per_section"
                        }
                        layout::SlideMode::Whole => "notebook.dashboard.settings.slides_whole",
                    }),
                )
                .tooltip(t("notebook.dashboard.settings.slides_tooltip"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let next = match slides {
                        layout::SlideMode::PerSection => layout::SlideMode::Whole,
                        layout::SlideMode::Whole => layout::SlideMode::PerSection,
                    };
                    this.set_dashboard_slides(next, cx)
                })),
            )
            .child(
                text_button("nb-dash-theme", theme_label)
                    .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                    .dropdown_menu(move |mut menu, _, _| {
                        for choice in [
                            layout::ThemeChoice::FollowApp,
                            layout::ThemeChoice::Light,
                            layout::ThemeChoice::Dark,
                        ] {
                            let this = this.clone();
                            menu = menu.item(
                                PopupMenuItem::new(t(theme_label_key(choice)))
                                    .checked(choice == theme)
                                    .on_click(move |_, _, app| {
                                        this.update(app, |v, cx| v.set_dashboard_theme(choice, cx))
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .child(
                text_button(
                    "nb-dash-logo",
                    t(if has_logo {
                        "notebook.dashboard.settings.logo_replace"
                    } else {
                        "notebook.dashboard.settings.logo_add"
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| this.pick_dashboard_logo(cx))),
            )
            .when(has_logo, |d| {
                d.child(
                    text_button(
                        "nb-dash-logo-remove",
                        t("notebook.dashboard.settings.logo_remove"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.remove_dashboard_logo(cx))),
                )
            })
            .into_any_element()
    }
}

/// Up to `cap` distinct values of `column` in `output`, in first-seen
/// order: the choices a drill-through offers.
pub fn column_values(
    output: &sqlly_services::notebook::CellOutput,
    column: &str,
    cap: usize,
) -> Vec<String> {
    let Some(index) = output
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(column))
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for row in output.rows.iter() {
        let text = row.get(index).map(value_text).unwrap_or_default();
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

/// Insert tile `id` right before tile `before` (same tab). Returns whether
/// anything moved.
fn place_before(spec: &mut LayoutSpec, id: &str, before: &str) -> bool {
    let (Some(from), Some(to)) = (spec.tile_pos(id), spec.tile_pos(before)) else {
        return false;
    };
    if (from.section, from.tab) != (to.section, to.tab) || from.tile == to.tile {
        return false;
    }
    let target = if from.tile < to.tile {
        to.tile - 1
    } else {
        to.tile
    };
    spec.move_tile(id, target as isize - from.tile as isize)
}

/// The parameters the filter bar shows: the dashboard's choice, or none.
fn filter_params(spec: &LayoutSpec, view: &NotebookView) -> Vec<String> {
    spec.filters
        .params
        .iter()
        .filter(|name| view.doc.model().parameters.iter().any(|p| p.name == **name))
        .cloned()
        .collect()
}

pub fn section_label(title: &str, index: usize) -> String {
    if title.trim().is_empty() {
        crate::localization::format(
            "notebook.dashboard.section.untitled",
            &[("number", index + 1)],
        )
    } else {
        title.to_string()
    }
}

pub fn tab_label(title: &str, index: usize) -> String {
    if title.trim().is_empty() {
        crate::localization::format("notebook.dashboard.tab.untitled", &[("number", index + 1)])
    } else {
        title.to_string()
    }
}

fn title_or_id(tile: &Tile, cell_name: Option<String>) -> String {
    tile.title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or(cell_name)
        .unwrap_or_else(|| tile.id.clone())
}

fn theme_label_key(theme: layout::ThemeChoice) -> &'static str {
    match theme {
        layout::ThemeChoice::FollowApp => "notebook.dashboard.settings.theme_follow",
        layout::ThemeChoice::Light => "notebook.dashboard.settings.theme_light",
        layout::ThemeChoice::Dark => "notebook.dashboard.settings.theme_dark",
    }
}

/// "Refreshing runs 2 queries on Storefront Dev, Sales."
pub fn refresh_text(plan: &RefreshPlan) -> String {
    if plan.cells.is_empty() {
        return t("notebook.dashboard.refresh.none");
    }
    let names: Vec<&str> = plan.connections.iter().map(|(l, _)| l.as_str()).collect();
    let production = plan.connections.iter().any(|(_, p)| *p);
    let key = match (production, plan.cells.len() == 1) {
        (true, true) => "notebook.dashboard.refresh.needed_production_one",
        (true, false) => "notebook.dashboard.refresh.needed_production",
        (false, true) => "notebook.dashboard.refresh.needed_one",
        (false, false) => "notebook.dashboard.refresh.needed",
    };
    crate::localization::format(
        key,
        &[
            ("count", plan.cells.len().to_string()),
            ("connections", names.join(", ")),
        ],
    )
}

fn heading(text: String) -> gpui::Div {
    div()
        .pt(px(Theme::scaled(4.0)))
        .text_color(Theme::text())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(text)
}

fn label(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text)
}

fn logo_image(logo: &layout::Logo) -> Option<std::sync::Arc<gpui::Image>> {
    let bytes = logo.bytes()?;
    let format = match logo.format {
        layout::LogoFormat::Png => gpui::ImageFormat::Png,
        layout::LogoFormat::Svg => gpui::ImageFormat::Svg,
    };
    Some(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
}

fn unreadable(message: String, cx: &mut Context<NotebookView>) -> AnyElement {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_col()
        .gap(px(s(10.0)))
        .p(px(s(24.0)))
        .child(div().text_color(Theme::warning()).child(message))
        .child(
            text_button(
                "nb-dash-unreadable-back",
                t("notebook.dashboard.present.unreadable_back"),
            )
            .on_click(cx.listener(|this, _, _, cx| this.close_dashboard(cx))),
        )
        .into_any_element()
}
