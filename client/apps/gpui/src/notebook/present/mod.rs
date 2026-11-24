//! Notebook modes (spec 4.2, 8.3): the dashboard editor and viewer, the
//! full-window presentation of a dashboard, the refusal shown when a
//! notebook with data-changing cells is opened as an app, and the zen and
//! dense switches of the notebook editor.
//!
//! Viewer and presentation are checked with
//! `notebook_security::authorize(Publish)` every time they are entered, so a
//! notebook that gains a write cell can no longer be shown as an app.
//! Presentation hides the cursor on every key press until the mouse moves
//! (the app's cursor-hide mode, set while presenting), can force the light
//! or dark theme the dashboard asks for, and restores the window, theme and
//! cursor behavior when it ends.
//!
//! The app's palette is one process-wide setting read while elements are
//! built (and by gpui-component through its global theme), so a forced theme
//! cannot be scoped to the presentation's subtree; it applies to the whole
//! app while presenting. [`ForcedTheme`] puts it back on every way out: Esc
//! or Stop, the tab or window closing (the guard drops with the view), and
//! another presentation ending first. It is never saved, so a crash while
//! presenting starts next time on the theme in Settings.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use gpui::{
    AnyElement, App, Context, CursorHideMode, Focusable as _, InteractiveElement as _, IntoElement,
    KeyBinding, MouseButton, MouseMoveEvent, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, Window, actions, div, prelude::FluentBuilder as _, px,
};
use sqlly_services::notebook::ExecClass;
use sqlly_services::notebook::cells::layout::{LayoutSpec, SlideMode, SlideRef, ThemeChoice};
use sqlly_services::notebook_security::authorize::{
    AuthContext, Consents, DataPolicy, Decision, NotebookAction, PublishMode, authorize,
};

use super::dashboard::{DashboardState, section_label, tab_label};
use super::rails::text_button;
use super::view::NotebookView;
use crate::theme::Theme;

actions!(
    notebook_presentation,
    [
        NextSlide,
        PreviousSlide,
        FirstSlide,
        LastSlide,
        ExitPresentation
    ]
);

/// Key context of the dashboard and presentation surfaces.
pub const KEY_CONTEXT: &str = "NotebookDashboard";
/// Added while presenting: slide keys and Esc.
pub const PRESENTATION_KEY_CONTEXT: &str = "NotebookPresentation";
/// Added in the viewer: Esc leaves it.
pub const VIEWER_KEY_CONTEXT: &str = "NotebookViewer";

/// The presentation keys. They are fixed: arrows, Page Up/Down, Space,
/// Home/End and Esc, as in any slide show.
pub fn register_keybindings(cx: &mut App) {
    let p = Some(PRESENTATION_KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("right", NextSlide, p),
        KeyBinding::new("down", NextSlide, p),
        KeyBinding::new("pagedown", NextSlide, p),
        KeyBinding::new("space", NextSlide, p),
        KeyBinding::new("left", PreviousSlide, p),
        KeyBinding::new("up", PreviousSlide, p),
        KeyBinding::new("pageup", PreviousSlide, p),
        KeyBinding::new("shift-space", PreviousSlide, p),
        KeyBinding::new("home", FirstSlide, p),
        KeyBinding::new("end", LastSlide, p),
        KeyBinding::new("escape", ExitPresentation, p),
        KeyBinding::new("escape", ExitPresentation, Some(VIEWER_KEY_CONTEXT)),
    ]);
}

/// What fills the notebook tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Surface {
    #[default]
    Notebook,
    Dashboard,
    Presentation,
    /// Viewer or presentation was refused; the reason is shown.
    Refused,
}

/// Why the viewer or a presentation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub presenting: bool,
    /// Where Back returns to.
    pub from: Surface,
    pub message: String,
    /// Names of the cells that change data.
    pub write_cells: Vec<String>,
}

/// What a presentation puts back when it ends.
#[derive(Debug)]
struct PresentSession {
    return_to: Surface,
    return_editing: bool,
    entered_fullscreen: bool,
    theme: Option<ForcedTheme>,
    cursor_mode: CursorHideMode,
}

/// The app theme before any presentation forced one, and the forced themes
/// of presentations still running, oldest first.
struct Forced {
    before: crate::theme::ThemeKind,
    held: Vec<(u64, crate::theme::ThemeKind)>,
    next: u64,
}

static FORCED: std::sync::Mutex<Option<Forced>> = std::sync::Mutex::new(None);

/// A presentation's forced theme, put back when dropped: to the newest
/// presentation still running, else to the theme from before the first.
/// A theme chosen in Settings while presenting is kept.
#[derive(Debug)]
pub(super) struct ForcedTheme {
    id: u64,
}

impl ForcedTheme {
    pub(super) fn force(kind: crate::theme::ThemeKind) -> ForcedTheme {
        let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
        let state = forced.get_or_insert_with(|| Forced {
            before: crate::theme::kind(),
            held: Vec::new(),
            next: 0,
        });
        let id = state.next;
        state.next += 1;
        state.held.push((id, kind));
        crate::theme::set_kind(kind);
        ForcedTheme { id }
    }
}

impl Drop for ForcedTheme {
    fn drop(&mut self) {
        let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = forced.as_mut() else {
            return;
        };
        let showing = state.held.last().map(|(_, kind)| *kind);
        state.held.retain(|(id, _)| *id != self.id);
        if showing != Some(crate::theme::kind()) {
            // Changed in Settings while presenting: that choice stays.
            *forced = None;
            return;
        }
        match state.held.last() {
            Some((_, kind)) => crate::theme::set_kind(*kind),
            None => {
                crate::theme::set_kind(state.before);
                *forced = None;
            }
        }
    }
}

#[derive(Default)]
pub struct PresentState {
    pub surface: Surface,
    pub dashboard: Option<DashboardState>,
    /// Hide the rails and the activity panel around the cells.
    pub zen: bool,
    /// Tighter cell spacing.
    pub dense: bool,
    /// Index into the dashboard's slides while presenting.
    pub slide: usize,
    pub refusal: Option<Refusal>,
    session: Option<PresentSession>,
}

impl PresentState {
    /// Whether the tab shows something other than the cell list.
    pub fn takes_over(&self) -> bool {
        self.surface != Surface::Notebook
    }

    /// Vertical padding around each cell, in unscaled pixels.
    pub fn cell_padding(&self) -> (f32, f32) {
        if self.dense { (6.0, 2.0) } else { (12.0, 5.0) }
    }
}

/// Whether cells of `notebook` draw only their outputs now (a dashboard,
/// viewer or presentation fills the tab): no toolbars, settings or SQL.
#[cfg(test)]
pub(crate) fn output_only(notebook: &gpui::WeakEntity<NotebookView>, cx: &App) -> bool {
    notebook
        .upgrade()
        .is_some_and(|n| n.read(cx).present.takes_over())
}

/// Whether cell `id` of `notebook` draws only its output now: a dashboard,
/// viewer or presentation fills the tab, or the cell is shown embedded.
pub(crate) fn cell_output_only(
    notebook: &gpui::WeakEntity<NotebookView>,
    id: &str,
    cx: &App,
) -> bool {
    notebook.upgrade().is_some_and(|n| {
        let n = n.read(cx);
        n.present.takes_over() || n.is_embedded(id)
    })
}

impl NotebookView {
    /// Tell the visual cell views whether they draw outputs only, and how
    /// (an embedded cell's configuration, height and locked view).
    pub(super) fn sync_output_only(&mut self, cx: &mut Context<Self>) {
        let takes_over = self.present.takes_over();
        for (id, slot) in &self.visual.slots {
            let (only, display) = self.output_display(id, takes_over);
            let view = slot.view.read(cx);
            if view.output_only != only || view.display != display {
                slot.view.update(cx, |v, cx| {
                    v.output_only = only;
                    v.display = display;
                    cx.notify();
                });
            }
        }
    }

    #[cfg(test)]
    pub fn surface(&self) -> Surface {
        self.present.surface
    }

    #[cfg(test)]
    pub fn refusal(&self) -> Option<&Refusal> {
        self.present.refusal.as_ref()
    }

    #[cfg(test)]
    pub fn current_slide(&self) -> usize {
        self.present.slide
    }

    /// Whether the notebook may be shown as an app now (`authorize` with
    /// [`PublishMode`]): refused while any cell changes data.
    pub fn publish_check(&self, mode: PublishMode) -> Result<(), Refusal> {
        let doc = self.doc.model();
        let trust = self.live_trust();
        let connections = BTreeMap::new();
        let ctx = AuthContext {
            doc,
            trust: &trust,
            connections: &connections,
            policy: &DataPolicy::default(),
            consents: &Consents::default(),
        };
        match authorize(&NotebookAction::Publish(mode), &ctx) {
            Decision::Allow { .. } => Ok(()),
            Decision::Deny(reason) | Decision::NeedsApproval(reason) => Err(Refusal {
                presenting: mode == PublishMode::Present,
                from: match self.present.surface {
                    Surface::Dashboard => Surface::Dashboard,
                    _ => Surface::Notebook,
                },
                message: crate::localization::message(reason.message()),
                write_cells: doc
                    .cells
                    .iter()
                    .filter(|c| {
                        c.kind.exec_class() == ExecClass::Remote && !c.execution_policy.read_only
                    })
                    .map(|c| c.relation_name().to_string())
                    .collect(),
            }),
        }
    }

    pub(crate) fn refuse(&mut self, refusal: Refusal, cx: &mut Context<Self>) {
        self.present.refusal = Some(refusal);
        self.present.surface = Surface::Refused;
        self.request_focus_root();
        cx.notify();
    }

    pub fn dismiss_refusal(&mut self, cx: &mut Context<Self>) {
        let from = self
            .present
            .refusal
            .take()
            .map(|r| r.from)
            .unwrap_or_default();
        self.present.surface = match (from, self.present.dashboard.as_mut()) {
            (Surface::Dashboard, Some(dash)) => {
                dash.editing = true;
                Surface::Dashboard
            }
            _ => Surface::Notebook,
        };
        self.request_focus_root();
        cx.notify();
    }

    pub fn toggle_zen(&mut self, cx: &mut Context<Self>) {
        self.present.zen = !self.present.zen;
        cx.notify();
    }

    pub fn toggle_dense(&mut self, cx: &mut Context<Self>) {
        self.present.dense = !self.present.dense;
        self.list_state.remeasure();
        cx.notify();
    }

    fn presentation_slides(&self) -> Vec<SlideRef> {
        let Some(dash) = self.present.dashboard.as_ref() else {
            return Vec::new();
        };
        self.layout_spec(&dash.cell)
            .map(|spec| spec.slides())
            .unwrap_or_default()
    }

    /// Present the dashboard in layout cell `cell` (or the first one) full
    /// window, one section tab per slide. Refused with the reason shown
    /// when the notebook has cells that change data.
    pub fn start_presentation(
        &mut self,
        cell: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(refusal) = self.publish_check(PublishMode::Present) {
            self.refuse(refusal, cx);
            return;
        }
        let id = self.layout_cell_or_create(cell, cx);
        let return_to = match self.present.surface {
            Surface::Presentation | Surface::Refused => Surface::Notebook,
            other => other,
        };
        let return_editing = self.present.dashboard.as_ref().is_some_and(|d| d.editing);
        let previous = self
            .present
            .dashboard
            .as_ref()
            .map(|d| (d.cell.clone(), d.slide));
        self.enter_dashboard(id.clone(), false, cx);
        let spec = self.layout_spec(&id).unwrap_or_default();
        let slides = spec.slides();
        self.present.slide = match previous {
            Some((cell, slide)) if cell == id => {
                slides.iter().position(|s| *s == slide).unwrap_or(0)
            }
            _ => 0,
        };
        if let Some(slide) = slides.get(self.present.slide).copied()
            && let Some(dash) = self.present.dashboard.as_mut()
        {
            dash.slide = slide;
        }
        let entered_fullscreen = !window.is_fullscreen();
        if entered_fullscreen {
            window.toggle_fullscreen();
        }
        let theme = forced_theme(spec.theme).map(|kind| {
            let forced = ForcedTheme::force(kind);
            cx.refresh_windows();
            forced
        });
        let cursor_mode = cx.cursor_hide_mode();
        cx.set_cursor_hide_mode(CursorHideMode::OnTypingAndAction);
        self.present.session = Some(PresentSession {
            return_to,
            return_editing,
            entered_fullscreen,
            theme,
            cursor_mode,
        });
        self.present.surface = Surface::Presentation;
        self.request_focus_root();
        cx.notify();
    }

    /// End the presentation and put the window, theme and cursor back.
    pub fn stop_presentation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.present.session.take() else {
            self.present.surface = Surface::Notebook;
            cx.notify();
            return;
        };
        if session.entered_fullscreen && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        if let Some(forced) = session.theme {
            drop(forced);
            cx.refresh_windows();
        }
        cx.set_cursor_hide_mode(session.cursor_mode);
        self.present.surface = session.return_to;
        if let Some(dash) = self.present.dashboard.as_mut() {
            dash.editing = session.return_editing;
        }
        self.request_focus_root();
        cx.notify();
    }

    pub fn go_to_slide(&mut self, index: usize, cx: &mut Context<Self>) {
        let slides = self.presentation_slides();
        if slides.is_empty() {
            return;
        }
        let index = index.min(slides.len() - 1);
        self.present.slide = index;
        let slide = slides[index];
        self.show_slide(slide, cx);
    }

    fn on_next_slide(&mut self, _: &NextSlide, _: &mut Window, cx: &mut Context<Self>) {
        if self.present.surface == Surface::Presentation {
            self.go_to_slide(self.present.slide + 1, cx);
        }
    }

    fn on_previous_slide(&mut self, _: &PreviousSlide, _: &mut Window, cx: &mut Context<Self>) {
        if self.present.surface == Surface::Presentation {
            self.go_to_slide(self.present.slide.saturating_sub(1), cx);
        }
    }

    fn on_first_slide(&mut self, _: &FirstSlide, _: &mut Window, cx: &mut Context<Self>) {
        if self.present.surface == Surface::Presentation {
            self.go_to_slide(0, cx);
        }
    }

    fn on_last_slide(&mut self, _: &LastSlide, _: &mut Window, cx: &mut Context<Self>) {
        if self.present.surface == Surface::Presentation {
            self.go_to_slide(usize::MAX, cx);
        }
    }

    fn on_exit_presentation(
        &mut self,
        _: &ExitPresentation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.present.surface {
            Surface::Presentation => self.stop_presentation(window, cx),
            Surface::Dashboard => self.close_dashboard(cx),
            Surface::Refused => self.dismiss_refusal(cx),
            Surface::Notebook => {}
        }
    }

    /// The whole tab while a dashboard, presentation or refusal shows.
    pub(super) fn render_takeover(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let surface = self.present.surface;
        let viewer = surface == Surface::Dashboard
            && self.present.dashboard.as_ref().is_some_and(|d| !d.editing);
        let content = match surface {
            Surface::Dashboard => self.render_dashboard_surface(window, cx),
            Surface::Presentation => self.render_presentation(window, cx),
            Surface::Refused => self.render_refusal(cx),
            Surface::Notebook => div().into_any_element(),
        };
        let mut context = gpui::KeyContext::default();
        context.add(KEY_CONTEXT);
        if surface == Surface::Presentation {
            context.add(PRESENTATION_KEY_CONTEXT);
        }
        if viewer || surface == Surface::Refused {
            context.add(VIEWER_KEY_CONTEXT);
        }
        let handle = self.focus_handle(cx);
        div()
            .id("notebook")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(Theme::bg())
            .key_context(context)
            .track_focus(&handle)
            .on_action(cx.listener(Self::on_next_slide))
            .on_action(cx.listener(Self::on_previous_slide))
            .on_action(cx.listener(Self::on_first_slide))
            .on_action(cx.listener(Self::on_last_slide))
            .on_action(cx.listener(Self::on_exit_presentation))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(resize) = this.dashboard().and_then(|d| d.resize.clone()) else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    this.end_tile_resize(cx);
                    return;
                }
                let position = if resize.width {
                    f32::from(event.position.x)
                } else {
                    f32::from(event.position.y)
                };
                this.resize_tile_to(position, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dashboard().is_some_and(|d| d.resize.is_some()) {
                        this.end_tile_resize(cx);
                    }
                }),
            )
            .child(content)
    }

    fn render_presentation(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let Some(cell) = self.present.dashboard.as_ref().map(|d| d.cell.clone()) else {
            return div().into_any_element();
        };
        let spec: LayoutSpec = self.layout_spec(&cell).unwrap_or_default();
        let slides = spec.slides();
        let index = self.present.slide.min(slides.len().saturating_sub(1));
        let title = if spec.title.trim().is_empty() {
            self.doc.title().unwrap_or_default().to_string()
        } else {
            spec.title.clone()
        };
        let body = if slides.is_empty() {
            div()
                .p(px(s(40.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "notebook.dashboard.present.empty",
                ))
                .into_any_element()
        } else if spec.presentation.slides == SlideMode::Whole {
            let mut all = div().flex().flex_col().w_full();
            for (n, section) in spec.sections.iter().enumerate() {
                for (m, tab) in section.tabs.iter().enumerate() {
                    let slide = SlideRef { section: n, tab: m };
                    all = all
                        .child(slide_heading(
                            &section.title,
                            n,
                            &tab.title,
                            m,
                            section.tabs.len(),
                        ))
                        .child(self.render_canvas(&spec, slide, false, window, cx));
                }
            }
            all.into_any_element()
        } else {
            let slide = slides[index];
            let section = &spec.sections[slide.section];
            let tab = &section.tabs[slide.tab];
            div()
                .flex()
                .flex_col()
                .w_full()
                .child(slide_heading(
                    &section.title,
                    slide.section,
                    &tab.title,
                    slide.tab,
                    section.tabs.len(),
                ))
                .child(self.render_canvas(&spec, slide, false, window, cx))
                .into_any_element()
        };
        let has_filters = !spec.filters.params.is_empty();
        div()
            .id("nb-presentation")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(Theme::bg())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(s(24.0)))
                    .pt(px(s(16.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(1.4)))
                            .text_color(Theme::text())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    ),
            )
            .when(has_filters, |d| {
                d.child(self.render_filter_bar(&spec, false, cx))
            })
            .child(
                div()
                    .id("nb-presentation-body")
                    .flex_1()
                    .min_h_0()
                    .px(px(s(18.0)))
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(12.0)))
                    .px(px(s(24.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .when(!slides.is_empty(), |d| {
                        d.child(crate::localization::format(
                            "notebook.dashboard.present.slide_counter",
                            &[
                                ("number", (index + 1).to_string()),
                                ("total", slides.len().to_string()),
                            ],
                        ))
                    })
                    .child(div().flex_1())
                    .child(crate::localization::text(
                        "notebook.dashboard.present.exit_hint",
                    )),
            )
            .into_any_element()
    }

    fn render_refusal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let Some(refusal) = self.present.refusal.clone() else {
            return div().into_any_element();
        };
        div()
            .id("nb-refusal")
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .bg(Theme::bg())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(10.0)))
                    .w(px(s(520.0)))
                    .p(px(s(20.0)))
                    .rounded(px(s(6.0)))
                    .border_1()
                    .border_color(Theme::error())
                    .bg(Theme::panel_bg())
                    .child(
                        div()
                            .text_color(Theme::text())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(crate::localization::text(if refusal.presenting {
                                "notebook.dashboard.refusal.title_present"
                            } else {
                                "notebook.dashboard.refusal.title_view"
                            })),
                    )
                    .child(
                        div()
                            .text_color(Theme::text())
                            .child(refusal.message.clone()),
                    )
                    .when(!refusal.write_cells.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.9)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::format(
                                    "notebook.dashboard.refusal.write_cells",
                                    &[("cells", refusal.write_cells.join(", "))],
                                )),
                        )
                    })
                    .child(
                        div().flex().flex_row().justify_end().child(
                            text_button(
                                "nb-refusal-back",
                                crate::localization::text("notebook.dashboard.refusal.back"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.dismiss_refusal(cx))),
                        ),
                    ),
            )
            .into_any_element()
    }
}

fn forced_theme(choice: ThemeChoice) -> Option<crate::theme::ThemeKind> {
    match choice {
        ThemeChoice::FollowApp => None,
        ThemeChoice::Light => Some(crate::theme::ThemeKind::Light),
        ThemeChoice::Dark => Some(crate::theme::ThemeKind::Dark),
    }
}

fn slide_heading(
    section: &str,
    section_index: usize,
    tab: &str,
    tab_index: usize,
    tabs: usize,
) -> gpui::Div {
    let s = Theme::scaled;
    let section = section_label(section, section_index);
    let text = if tabs > 1 {
        crate::localization::format(
            "notebook.dashboard.present.section_tab",
            &[
                ("section", section.as_str()),
                ("tab", tab_label(tab, tab_index).as_str()),
            ],
        )
    } else {
        section
    };
    div()
        .px(px(s(8.0)))
        .pt(px(s(10.0)))
        .text_size(px(Theme::font_size_ratio(1.1)))
        .text_color(Theme::text_dim())
        .child(text)
}
