//! Embedded outputs: a cell shown with only its output — no header, editor,
//! results chrome or settings — sitting flush with its neighbours, so a
//! notebook reads like a report. The setting lives in the document
//! (`sqlly_services::notebook::display`): per cell, with a notebook-wide
//! default ("Embed all outputs"). Each change is one undo step; none of it
//! runs anything or touches the trust hash.
//!
//! Dashboard tiles, the viewer and presentations draw outputs through the
//! same [`NotebookView::output_render_cx`] and `render_output`, so a cell's
//! output looks the same everywhere it is shown alone.
//!
//! Getting back: hovering an embedded cell (or selecting it) shows a small
//! control with Show in full, Show or hide configuration (pivot and chart
//! cells), Run and the cell menu; `E` in command mode toggles the selected
//! cells; "Embed all outputs" is in the More menu and the palette.

pub mod grid_view;
pub mod keys;

#[cfg(test)]
mod tests;

use std::cell::Cell as StdCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, Styled as _, Window, canvas, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use sqlly_services::notebook::NotebookDoc;
use sqlly_services::notebook::display::{self, DisplayMode, LockedView, ResolvedDisplay};

use super::cells::{self, CellRenderCx};
use super::doc_adapter::{KIND_MARKDOWN, KIND_SQL};
use super::rails::icon_button;
use super::view::{CellStatus, NotebookView, status_label_key};
use super::visual::OutputDisplay;
use crate::icon_catalog::AppIcon;
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

/// The embedded-output commands (keyboard, palette, menus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddedCommand {
    /// Embed the selected cells, or show them in full again.
    Toggle,
    /// Flip the notebook default for every cell.
    EmbedAll,
    /// Show or hide the active pivot or chart cell's configuration.
    ToggleConfig,
}

/// View-side state: the last drawn height of each embedded output, where a
/// resize starts from.
#[derive(Default)]
pub struct EmbeddedState {
    measured: HashMap<String, Rc<StdCell<f32>>>,
    /// Results grids whose chrome a cell changed (see [`grid_view::apply`]).
    touched: HashSet<gpui::EntityId>,
    /// The rows behind a drill in a grid that cannot show them itself.
    drills: HashMap<String, grid_view::DrillRows>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BadgeTone {
    Warning,
    Error,
    Busy,
}

/// The status mark an embedded output carries, if any: everything but a
/// current result (or a cell that never ran) is marked, so a stale,
/// partial, failed or running output never passes for a current one.
fn badge(state: CellStatus, stale: bool) -> Option<BadgeTone> {
    let current = matches!(state, CellStatus::Fresh | CellStatus::Cached) && !stale;
    if current || state == CellStatus::Idle {
        return None;
    }
    let tone = match state {
        _ if stale => BadgeTone::Warning,
        CellStatus::Failed | CellStatus::Blocked | CellStatus::Unavailable => BadgeTone::Error,
        _ if state.is_active() => BadgeTone::Busy,
        _ => BadgeTone::Warning,
    };
    Some(tone)
}

/// The menu label of a locked-view choice.
fn lock_label(lock: Option<LockedView>) -> String {
    match lock {
        None => t("notebook.embedded.menu.lock_off"),
        Some(LockedView::Rows) => t("notebook.embedded.menu.lock_rows"),
        Some(LockedView::Pivot) => t("notebook.embedded.menu.lock_pivot"),
        Some(LockedView::Chart) => t("notebook.embedded.menu.lock_chart"),
    }
}

/// Kinds whose embedded output has a configuration to show or hide.
fn configurable(kind: &str) -> bool {
    matches!(kind, "pivot" | "chart")
}

/// Kinds whose output is a results grid that can be locked to one view.
fn lockable(kind: &str) -> bool {
    matches!(kind, KIND_SQL | "table")
}

/// Kinds whose embedded output can be resized.
fn resizable(kind: &str) -> bool {
    matches!(kind, KIND_SQL | "table" | "pivot" | "chart")
}

impl NotebookView {
    /// Cell `id`'s display settings with the notebook default applied.
    pub(crate) fn display_of(&self, id: &str) -> ResolvedDisplay {
        let doc = self.doc.model();
        doc.cell(id)
            .map(|c| display::resolve(&doc.settings, c))
            .unwrap_or_default()
    }

    /// Whether cell `id` draws only its output in the notebook.
    pub(crate) fn is_embedded(&self, id: &str) -> bool {
        self.display_of(id).embedded()
    }

    /// Whether the notebook default is embedded ("Embed all outputs").
    pub(crate) fn embeds_all(&self) -> bool {
        display::default_mode(&self.doc.model().settings) == DisplayMode::Embedded
    }

    /// The output height to draw `id` at: the one being dragged, else the
    /// one the user chose; `None` fits the content.
    fn output_height(&self, id: &str) -> Option<f32> {
        if self.resizing_output_of() == Some(id) {
            return self.cells.get(id).map(|rt| rt.results_height);
        }
        self.display_of(id).height
    }

    /// Whether a visual cell draws only its output, and how.
    pub(super) fn output_display(&self, id: &str, takes_over: bool) -> (bool, OutputDisplay) {
        let shown = self.display_of(id);
        let embedded = !takes_over && shown.embedded();
        (
            takes_over || embedded,
            OutputDisplay {
                config_shown: embedded && shown.config_shown,
                height: embedded.then(|| self.output_height(id)).flatten(),
                lock: shown.lock,
            },
        )
    }

    // ── Changes (each one undo step) ──────────────────────────────────────

    fn change_display(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut NotebookDoc) -> bool,
        cx: &mut Context<Self>,
    ) {
        if !self.doc.edit_model(edit) {
            return;
        }
        self.editing_label(label);
        self.list_state.remeasure();
        self.mark_dirty(cx);
    }

    /// Embed `ids`, or show them in full again when all of them are
    /// embedded.
    pub(crate) fn toggle_embedded(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        self.change_display(
            "notebook.embedded.step.toggle",
            |doc| {
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                display::toggle_mode(doc, &ids).is_some()
            },
            cx,
        );
    }

    /// Show `ids` in `mode`.
    pub(crate) fn set_display_mode(
        &mut self,
        ids: Vec<String>,
        mode: DisplayMode,
        cx: &mut Context<Self>,
    ) {
        self.change_display(
            "notebook.embedded.step.toggle",
            |doc| {
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                display::set_mode(doc, &ids, mode)
            },
            cx,
        );
    }

    /// "Embed all outputs": flip the notebook default; every cell follows.
    pub(crate) fn toggle_embed_all(&mut self, cx: &mut Context<Self>) {
        let mode = if self.embeds_all() {
            DisplayMode::Full
        } else {
            DisplayMode::Embedded
        };
        self.change_display(
            "notebook.embedded.step.embed_all",
            |doc| {
                display::set_all(doc, mode);
                true
            },
            cx,
        );
    }

    /// Show or hide an embedded pivot's or chart's configuration.
    pub(crate) fn toggle_embedded_config(&mut self, id: &str, cx: &mut Context<Self>) {
        let shown = self.display_of(id).config_shown;
        let id = id.to_string();
        self.change_display(
            "notebook.embedded.step.config",
            |doc| {
                doc.cells
                    .iter_mut()
                    .find(|c| c.id == id)
                    .map(|cell| display::update(cell, |d| d.config_shown = Some(!shown)))
                    .is_some()
            },
            cx,
        );
    }

    /// Lock `id`'s results grid to one view, or let it switch again.
    pub(crate) fn set_locked_view(
        &mut self,
        id: &str,
        lock: Option<LockedView>,
        cx: &mut Context<Self>,
    ) {
        let id = id.to_string();
        self.change_display(
            "notebook.embedded.step.lock",
            |doc| {
                doc.cells
                    .iter_mut()
                    .find(|c| c.id == id)
                    .map(|cell| display::update(cell, |d| d.lock = lock))
                    .is_some()
            },
            cx,
        );
    }

    /// A drag on an embedded output's edge ended: keep its height.
    pub(super) fn embedded_resize_ended(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.is_embedded(id) {
            return;
        }
        let Some(height) = self.cells.get(id).map(|rt| rt.results_height) else {
            return;
        };
        let id = id.to_string();
        self.change_display(
            "notebook.embedded.step.resize",
            |doc| {
                doc.cells
                    .iter_mut()
                    .find(|c| c.id == id)
                    .map(|cell| display::update(cell, |d| d.height = Some(height)))
                    .is_some()
            },
            cx,
        );
    }

    fn begin_embedded_resize(&mut self, id: &str, y: f32, cx: &mut Context<Self>) {
        let start = self
            .output_height(id)
            .or_else(|| self.embedded.measured.get(id).map(|m| m.get()))
            .unwrap_or(super::view::DEFAULT_RESULTS_HEIGHT);
        self.cells.entry(id.to_string()).or_default().results_height = start;
        self.begin_results_drag(id, y, cx);
    }

    pub(crate) fn embedded_command(&mut self, command: EmbeddedCommand, cx: &mut Context<Self>) {
        match command {
            EmbeddedCommand::Toggle => {
                let ids = self.selection();
                if ids.is_empty() {
                    self.status(t("notebook.shell.run.no_active_cell"), false, cx);
                    return;
                }
                self.toggle_embedded(ids, cx);
            }
            EmbeddedCommand::EmbedAll => self.toggle_embed_all(cx),
            EmbeddedCommand::ToggleConfig => {
                let Some(id) = self.active.clone() else {
                    return;
                };
                let kind = self.doc.cell_by_id(&id).map(|c| c.kind.to_string());
                if kind.as_deref().is_some_and(configurable) && self.is_embedded(&id) {
                    self.toggle_embedded_config(&id, cx);
                }
            }
        }
    }

    // ── The shared output path ────────────────────────────────────────────

    /// What a cell's renderer needs to draw its output alone: no editor,
    /// preview or settings. Dashboard tiles, the viewer, presentations and
    /// embedded cells all draw through this.
    pub(crate) fn output_render_cx(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<CellRenderCx> {
        let (index, kind, source) = self.doc.cell_by_id(id).map(|c| {
            (
                self.doc.index_of(id).unwrap_or(0),
                c.kind.to_string(),
                c.source.to_string(),
            )
        })?;
        let visual = self.prepare_visual(id, &kind, window, cx);
        let analysis = self.analysis_view(id, &kind, window, cx);
        let markdown_images = (kind == KIND_MARKDOWN)
            .then(|| self.markdown_images(&source, cx))
            .flatten();
        let rt = self.cells.get(id);
        Some(CellRenderCx {
            id: id.to_string(),
            index,
            kind: kind.clone(),
            source,
            active: self.active.as_deref() == Some(id),
            collapsed: false,
            state: self.doc.status(id),
            stale: self.is_stale(id),
            editor: None,
            results: rt.and_then(|rt| rt.results.clone()),
            results_height: self.output_height(id).unwrap_or(0.0),
            preview: None,
            editing: false,
            metrics: self.metrics.clone(),
            elapsed_ms: rt.and_then(|rt| rt.elapsed_ms),
            message: rt.and_then(|rt| rt.message_text()),
            attachment: (kind == super::doc_adapter::KIND_ATTACHMENT)
                .then(|| self.attachment_cx(id).map(Rc::new))
                .flatten(),
            output_only: true,
            visual,
            interpolated: (kind == KIND_MARKDOWN)
                .then(|| self.interpolate_markdown(id))
                .flatten(),
            markdown_images,
            analysis,
            ai: (kind == super::ai::KIND_AI).then(|| self.ai_snapshot(id, window, cx)),
            spec: self
                .doc
                .model()
                .cell(id)
                .map(|c| c.spec.clone())
                .unwrap_or_default(),
        })
    }

    /// Hold a SQL cell's results grid to the cell's locked view (and, when
    /// embedded, to as little chrome as the grid allows).
    pub(super) fn apply_grid_chrome(
        &mut self,
        cell: &CellRenderCx,
        bare: bool,
        cx: &mut Context<Self>,
    ) {
        if cell.kind != KIND_SQL {
            return;
        }
        let lock = self.display_of(&cell.id).lock;
        if lock.is_none() && !bare && self.embedded.touched.is_empty() {
            return;
        }
        let Some(results) = cell.results.clone() else {
            return;
        };
        let Some(grid) = results.update(cx, |r, cx| r.embedded_grid(cx)) else {
            return;
        };
        let weak = cx.weak_entity();
        let id = cell.id.clone();
        let source = grid.downgrade();
        let drill = move || -> sqlly_datatable::RowSourceDrillHandler {
            Rc::new(move |drill, app| {
                let Some(source) = source.upgrade() else {
                    return;
                };
                let rows = grid_view::DrillRows::of(&source, &drill, app);
                weak.update(app, |this, cx| this.show_drill_rows(&id, rows, cx))
                    .ok();
            })
        };
        grid_view::apply(
            &grid,
            grid_view::GridChrome { bare, lock },
            &mut self.embedded.touched,
            drill,
            cx,
        );
    }

    fn show_drill_rows(
        &mut self,
        id: &str,
        rows: Option<grid_view::DrillRows>,
        cx: &mut Context<Self>,
    ) {
        match rows {
            Some(rows) => {
                self.embedded.drills.insert(id.to_string(), rows);
            }
            None => {
                self.embedded.drills.remove(id);
            }
        }
        self.remeasure_cell(id);
        cx.notify();
    }

    /// The rows behind the last drill in `id`'s grid, with a Close button.
    pub(super) fn render_drill_rows(&self, id: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.embedded.drills.get(id)?;
        let close_id = id.to_string();
        let close = super::rails::text_button(
            format!("nb-drill-close-{id}"),
            t("notebook.embedded.drill.close"),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.show_drill_rows(&close_id, None, cx)));
        let selector = format!("nb-drill-rows-{id}");
        Some(
            div()
                .debug_selector(move || selector.clone())
                .w_full()
                .child(rows.render(close, cx))
                .into_any_element(),
        )
    }

    // ── Drawing ───────────────────────────────────────────────────────────

    /// An embedded cell in the list: its output flush with its neighbours,
    /// small status badges over it, the hover control, and a resize edge.
    pub(super) fn render_embedded_cell(
        &mut self,
        ix: usize,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(render_cx) = self.output_render_cx(id, window, cx) else {
            return div().into_any_element();
        };
        self.apply_grid_chrome(&render_cx, true, cx);
        let renderer = cells::renderer_for(&render_cx.kind);
        let body = renderer.render_output(&render_cx, window, cx);
        let badge = self.render_embedded_badge(&render_cx);
        let control = self.render_embedded_control(&render_cx, renderer, cx);
        let measured = self
            .embedded
            .measured
            .entry(id.to_string())
            .or_default()
            .clone();
        let s = Theme::scaled;
        let (pad_x, _) = self.present.cell_padding();
        let selected = self.selected.iter().any(|s| s == id);
        let group: SharedString = format!("nb-embedded-{ix}").into();
        let click_id = id.to_string();
        let resize_id = id.to_string();
        let selector = format!("nb-embedded-{id}");
        let frame = div()
            .relative()
            .w_full()
            .rounded(px(s(3.0)))
            .border_1()
            .border_color(if render_cx.active {
                Theme::accent()
            } else if selected {
                Theme::border_hover()
            } else {
                gpui::transparent_black()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.click_cell(&click_id, event.modifiers, cx);
                }),
            )
            .child(
                div().relative().w_full().child(body).child(
                    canvas(
                        move |b, _, _| measured.set(f32::from(b.size.height)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                ),
            )
            .children(self.render_drill_rows(id, cx))
            .children(badge)
            .child(control)
            .when(resizable(&render_cx.kind), |d| {
                d.child(
                    div()
                        .id(("nb-embedded-resize", ix))
                        .h(px(s(4.0)))
                        .w_full()
                        .cursor(CursorStyle::ResizeRow)
                        .hover(|style| style.bg(Theme::border_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                this.begin_embedded_resize(
                                    &resize_id,
                                    f32::from(event.position.y),
                                    cx,
                                );
                                cx.stop_propagation();
                            }),
                        ),
                )
            });
        div()
            .id(("nb-cell", ix))
            .debug_selector(move || selector.clone())
            .group(group)
            .w_full()
            .px(px(s(pad_x)))
            .py(px(s(1.0)))
            .child(frame)
            .into_any_element()
    }

    /// The honesty marks an embedded output keeps: running, failed,
    /// partial, stale, canceled — anything but a current result.
    fn render_embedded_badge(&self, cell: &CellRenderCx) -> Option<AnyElement> {
        let tone = badge(cell.state, cell.stale)?;
        let color = match tone {
            BadgeTone::Warning => Theme::warning(),
            BadgeTone::Error => Theme::error(),
            BadgeTone::Busy => Theme::accent(),
        };
        let s = Theme::scaled;
        let selector = format!("nb-embedded-badge-{}", cell.id);
        Some(
            div()
                .debug_selector(move || selector.clone())
                .absolute()
                .bottom(px(s(8.0)))
                .right(px(s(8.0)))
                .px(px(s(6.0)))
                .py(px(s(1.0)))
                .rounded(px(s(3.0)))
                .border_1()
                .border_color(color)
                .bg(Theme::panel_bg())
                .text_size(px(Theme::font_size_ratio(0.78)))
                .text_color(color)
                .child(crate::localization::text(status_label_key(
                    cell.state, cell.stale,
                )))
                .into_any_element(),
        )
    }

    /// The control an embedded cell shows on hover or while selected:
    /// Show in full, Show/Hide configuration, Run, and the cell menu.
    fn render_embedded_control(
        &mut self,
        cell: &CellRenderCx,
        renderer: &'static (dyn cells::NotebookCell + Sync),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let index = cell.index;
        let name = self
            .doc
            .cell_by_id(&cell.id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| cell.id.clone());
        let group: SharedString = format!("nb-embedded-{index}").into();
        let full_id = cell.id.clone();
        let mut bar = div()
            .absolute()
            .top(px(s(4.0)))
            .right(px(s(4.0)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(2.0)))
            .px(px(s(2.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg())
            .when(!cell.active, |d| {
                d.invisible().group_hover(group, |style| style.visible())
            })
            .child(
                icon_button(
                    format!("nb-embedded-full-{index}"),
                    AppIcon::Frame,
                    crate::localization::format(
                        "notebook.embedded.control.show_full",
                        &[("cell_name", name.as_str())],
                    ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_display_mode(vec![full_id.clone()], DisplayMode::Full, cx);
                })),
            );
        if configurable(&cell.kind) {
            let shown = self.display_of(&cell.id).config_shown;
            let id = cell.id.clone();
            bar = bar.child(
                icon_button(
                    format!("nb-embedded-config-{index}"),
                    AppIcon::SidebarRight,
                    t(if shown {
                        "notebook.embedded.control.hide_config"
                    } else {
                        "notebook.embedded.control.show_config"
                    }),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_embedded_config(&id, cx))),
            );
        }
        if renderer.runnable() {
            let id = cell.id.clone();
            bar = bar.child(
                icon_button(
                    format!("nb-embedded-run-{index}"),
                    AppIcon::Play,
                    crate::localization::format(
                        "notebook.embedded.control.run",
                        &[("cell_name", name.as_str())],
                    ),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.request_run_from_host(&id, cx))),
            );
        }
        bar.child(self.editing_cell_menu(cell, cx))
            .into_any_element()
    }

    /// The display items of a cell's menu, built before the menu opens.
    pub(crate) fn embedded_menu(&self, cell: &CellRenderCx) -> EmbeddedMenu {
        let shown = self.display_of(&cell.id);
        EmbeddedMenu {
            id: cell.id.clone(),
            embeddable: self
                .doc
                .model()
                .cell(&cell.id)
                .is_some_and(|c| display::embeddable(&c.kind)),
            embedded: shown.embedded(),
            configurable: configurable(&cell.kind),
            config_shown: shown.config_shown,
            lockable: lockable(&cell.kind),
            lock: shown.lock,
        }
    }
}

/// A cell menu's display items: embed or show in full, the configuration
/// of an embedded pivot or chart, and the view a results grid is locked to.
#[derive(Clone)]
pub struct EmbeddedMenu {
    id: String,
    embeddable: bool,
    embedded: bool,
    configurable: bool,
    config_shown: bool,
    lockable: bool,
    lock: Option<LockedView>,
}

impl EmbeddedMenu {
    pub(crate) fn add_to(
        &self,
        mut menu: PopupMenu,
        this: &gpui::Entity<NotebookView>,
    ) -> PopupMenu {
        if !self.embeddable {
            return menu;
        }
        menu = menu.separator();
        {
            let this = this.clone();
            let id = self.id.clone();
            menu = menu.item(
                PopupMenuItem::new(t(if self.embedded {
                    "notebook.embedded.menu.show_full"
                } else {
                    "notebook.embedded.menu.embed"
                }))
                .on_click(move |_, _, app| {
                    this.update(app, |v, cx| {
                        v.target_cell_for_menu(&id, cx);
                        let ids = v.selection();
                        v.toggle_embedded(ids, cx);
                    })
                }),
            );
        }
        if self.configurable && self.embedded {
            let this = this.clone();
            let id = self.id.clone();
            menu = menu.item(
                PopupMenuItem::new(t(if self.config_shown {
                    "notebook.embedded.menu.hide_config"
                } else {
                    "notebook.embedded.menu.show_config"
                }))
                .on_click(move |_, _, app| {
                    this.update(app, |v, cx| v.toggle_embedded_config(&id, cx))
                }),
            );
        }
        if self.lockable {
            let choices = [
                None,
                Some(LockedView::Rows),
                Some(LockedView::Pivot),
                Some(LockedView::Chart),
            ];
            for lock in choices {
                let this = this.clone();
                let id = self.id.clone();
                menu = menu.item(
                    PopupMenuItem::new(lock_label(lock))
                        .checked(self.lock == lock)
                        .on_click(move |_, _, app| {
                            this.update(app, |v, cx| v.set_locked_view(&id, lock, cx))
                        }),
                );
            }
        }
        menu
    }
}
