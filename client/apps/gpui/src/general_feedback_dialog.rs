//! "Send Feedback" dialog: a message box plus the app screenshot on an
//! annotation canvas (arrows and privacy blackouts).
//!
//! Always opened detached, in its own OS window (`dialog::open_detached` +
//! the `windowed` [`Dialog`] presentation), so the user can keep looking at
//! — and, if they like, re-trigger — the workbench it describes. The capture
//! is taken by the app *before* this window opens (see
//! `app::general_feedback`), so the dialog itself never appears in it.
//!
//! The canvas shows the screenshot scaled to fit; annotations live in
//! screenshot pixel space (`feedback_annotate`), so the fit scale only
//! affects painting and pointer mapping. What is painted here is exactly what
//! Send burns into the PNG: the same arrow geometry, and blackouts drawn on
//! top of everything.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, BorderStyle, Bounds, Context, CursorStyle, DispatchPhase, Entity, EventEmitter,
    FocusHandle, Focusable as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, RenderImage, Styled, Window,
    canvas, div, fill, outline, point, prelude::*, px, size,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Textarea, TextareaState};
use gpui_component::{Disableable as _, Selectable as _, Sizable as _};

use crate::dialog::{Dialog, DialogFrame};
use crate::feedback_annotate::{
    self as annotate, Annotation, AnnotationDoc, DragOp, Handle, Pt, Tool,
};
use crate::general_feedback::{FeedbackContext, SubmittedFeedback};
use crate::theme::Theme;

/// Smallest on-screen arrow stroke in the preview, in points. Badges scale
/// with it (a ~22 pt disc), so the digits stay legible however far the
/// capture is scaled down to fit.
const MIN_PREVIEW_ARROW_PT: f32 = 4.5;

/// The arrow width, in screenshot pixels, the preview paints with at display
/// `scale` (screen points per screenshot pixel): the burned-in width, or
/// wider when that would show thinner than [`MIN_PREVIEW_ARROW_PT`].
fn preview_arrow_width(arrow_width: f32, scale: f32) -> f32 {
    if scale <= 0.0 {
        return arrow_width;
    }
    arrow_width.max(MIN_PREVIEW_ARROW_PT / scale)
}

/// How much larger the preview draws arrow number badges than the
/// arrow-relative size burned into the upload, so `#N` reads at a glance.
const PREVIEW_BADGE_SCALE: f32 = 3.0;

/// The width, in screenshot pixels, that sizes the preview's number badges:
/// [`PREVIEW_BADGE_SCALE`]× the burned-in arrow width, and never smaller
/// on screen than a readable arrow ([`preview_arrow_width`]).
fn preview_badge_width(arrow_width: f32, scale: f32) -> f32 {
    (arrow_width * PREVIEW_BADGE_SCALE).max(preview_arrow_width(arrow_width, scale))
}

/// A capture ready for the canvas: the straight RGBA pixels (what gets
/// flattened and uploaded) plus a BGRA copy for GPUI's image painter.
pub struct CapturedScreenshot {
    pub rgba: Arc<image::RgbaImage>,
    render: Arc<RenderImage>,
    pub scale_factor: f32,
}

impl CapturedScreenshot {
    /// Prepare a capture for display. Copies the pixels once (GPUI samples
    /// images as BGRA), so run it off the UI thread.
    pub fn new(shot: crate::screenshot::Screenshot) -> Self {
        let mut bgra = shot.image.clone();
        for px in bgra.pixels_mut() {
            px.0.swap(0, 2);
        }
        Self {
            rgba: Arc::new(shot.image),
            render: Arc::new(RenderImage::new([image::Frame::new(bgra)])),
            scale_factor: shot.scale_factor,
        }
    }

    fn size(&self) -> (f32, f32) {
        (self.rgba.width() as f32, self.rgba.height() as f32)
    }
}

pub enum GeneralFeedbackEvent {
    /// Send was pressed: the host flattens, persists, and uploads.
    Submitted(Box<SubmittedFeedback>),
}

/// Canvas placement of the screenshot, recorded at paint time: window
/// position of the image's top-left and window pixels per image pixel.
#[derive(Clone, Copy, Debug)]
struct CanvasLayout {
    origin_x: f32,
    origin_y: f32,
    scale: f32,
}

impl CanvasLayout {
    fn to_image(self, p: gpui::Point<Pixels>) -> Pt {
        Pt::new(
            (f32::from(p.x) - self.origin_x) / self.scale,
            (f32::from(p.y) - self.origin_y) / self.scale,
        )
    }
}

/// On-screen handle / grab size in window pixels.
const HANDLE_PX: f32 = 8.0;

pub struct GeneralFeedbackDialog {
    screenshot: Option<CapturedScreenshot>,
    /// Why there is no screenshot (capture failed / unsupported platform).
    unavailable: Option<String>,
    doc: AnnotationDoc,
    tool: Tool,
    drag: Option<DragOp>,
    include_screenshot: bool,
    input: Option<Entity<TextareaState>>,
    error: Option<String>,
    context: FeedbackContext,
    frame: DialogFrame,
    canvas_focus: FocusHandle,
    layout: Rc<Cell<Option<CanvasLayout>>>,
}

impl EventEmitter<GeneralFeedbackEvent> for GeneralFeedbackDialog {}

impl GeneralFeedbackDialog {
    pub fn new(
        screenshot: Result<CapturedScreenshot, String>,
        context: FeedbackContext,
        cx: &mut Context<Self>,
    ) -> Self {
        let (screenshot, unavailable) = match screenshot {
            Ok(shot) => (Some(shot), None),
            Err(reason) => (None, Some(reason)),
        };
        let doc = match &screenshot {
            Some(shot) => AnnotationDoc::new(shot.rgba.width(), shot.rgba.height()),
            None => AnnotationDoc::new(0, 0),
        };
        Self {
            include_screenshot: screenshot.is_some(),
            screenshot,
            unavailable,
            doc,
            tool: Tool::Arrow,
            drag: None,
            input: None,
            error: None,
            context,
            frame: DialogFrame::new(Theme::scaled(1100.0), Theme::scaled(780.0)),
            canvas_focus: cx.focus_handle(),
            layout: Rc::new(Cell::new(None)),
        }
    }

    /// Open the dialog in its own window. `None` when the window could not
    /// be created.
    pub fn open(
        screenshot: Result<CapturedScreenshot, String>,
        context: FeedbackContext,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        crate::dialog::open_detached(
            crate::dialog::DetachedOptions::localized(
                "feedback.general.dialog_title",
                Theme::scaled(1100.0),
                Theme::scaled(780.0),
            ),
            None,
            cx,
            move |_window, cx| cx.new(|cx| Self::new(screenshot, context, cx)),
        )
    }

    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx).auto_grow(3, 6).placeholder(
                crate::localization::input_placeholder(
                    window,
                    cx,
                    "feedback.general.message_placeholder",
                ),
            )
        });
        input.focus_handle(cx).focus(window, cx);
        self.input = Some(input);
    }

    /// Annotation is possible: there is a screenshot and it will be sent.
    fn annotating(&self) -> bool {
        self.screenshot.is_some() && self.include_screenshot
    }

    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.tool = tool;
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if self.doc.undo() {
            cx.notify();
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if self.doc.redo() {
            cx.notify();
        }
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.doc.delete_selected() {
            cx.notify();
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self
            .input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let screenshot = self
            .screenshot
            .as_ref()
            .filter(|_| self.include_screenshot)
            .map(|shot| (shot.rgba.clone(), shot.scale_factor));
        if let Err(problem) =
            crate::general_feedback::validate_message(&message, screenshot.is_some())
        {
            self.error = Some(problem);
            cx.notify();
            return;
        }
        let annotations = if screenshot.is_some() {
            self.doc.items().to_vec()
        } else {
            Vec::new()
        };
        cx.emit(GeneralFeedbackEvent::Submitted(Box::new(
            SubmittedFeedback {
                message,
                screenshot,
                annotations,
                context: self.context.clone(),
            },
        )));
        // Close on the next effect cycle, not now: removing the window drops
        // this dialog, and GPUI releases a dropped entity's subscribers
        // before it delivers the queued `Submitted`, which silently lost the
        // report.
        window.defer(cx, |window, _| window.remove_window());
    }

    fn cancel(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    /// Grab slop in image pixels for the current fit scale.
    fn tolerance(&self, layout: CanvasLayout) -> f32 {
        HANDLE_PX / layout.scale.max(0.0001)
    }

    fn on_canvas_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.canvas_focus.focus(window, cx);
        if !self.annotating() {
            return;
        }
        let Some(layout) = self.layout.get() else {
            return;
        };
        let p = layout.to_image(ev.position);
        self.drag = self.doc.begin_drag(self.tool, p, self.tolerance(layout));
        cx.notify();
    }

    fn on_canvas_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let (Some(op), Some(layout)) = (self.drag, self.layout.get()) else {
            return;
        };
        if !ev.dragging() {
            // The button was released outside any listener; finish here.
            self.doc.end_drag(&op);
            self.drag = None;
            cx.notify();
            return;
        }
        self.doc.update_drag(&op, layout.to_image(ev.position));
        cx.notify();
    }

    fn on_canvas_up(&mut self, cx: &mut Context<Self>) {
        if let Some(op) = self.drag.take() {
            self.doc.end_drag(&op);
            cx.notify();
        }
    }

    fn on_canvas_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &ev.keystroke;
        let secondary = crate::keybindings::primary_modifier(&keystroke.modifiers);
        match keystroke.key.as_str() {
            "backspace" | "delete" => self.delete_selected(cx),
            "z" if secondary && keystroke.modifiers.shift => self.redo(cx),
            "z" if secondary => self.undo(cx),
            "y" if secondary => self.redo(cx),
            "v" if !secondary => self.set_tool(Tool::Select, cx),
            "a" if !secondary => self.set_tool(Tool::Arrow, cx),
            "b" if !secondary => self.set_tool(Tool::Blackout, cx),
            "escape" => {
                self.doc.select(None);
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_canvas(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let small = px(Theme::font_size_ratio(0.85));
        let Some(shot) = &self.screenshot else {
            let reason = self.unavailable.clone().unwrap_or_else(|| {
                crate::localization::text("feedback.general.no_screenshot_captured")
            });
            return div()
                .flex_1()
                .min_h(px(s(120.0)))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(Theme::border())
                .bg(Theme::panel_bg())
                .text_size(small)
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "feedback.general.screenshot_unavailable",
                    &[("reason", &reason)],
                ))
                .into_any_element();
        };

        let layout_cell = self.layout.clone();
        let render = shot.render.clone();
        let (iw, ih) = shot.size();
        let scale_factor = shot.scale_factor.max(1.0);
        let items = self.doc.items().to_vec();
        let selected = self.doc.selected();
        let arrow_width = annotate::arrow_width_for_scale(shot.scale_factor);
        let excluded = !self.include_screenshot;
        let dragging = self.drag.is_some();
        let entity = cx.entity();

        let painter = canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                let bw = f32::from(bounds.size.width);
                let bh = f32::from(bounds.size.height);
                if dragging {
                    // Track the drag window-wide (not just while hovering
                    // the canvas) so a drag that leaves the canvas keeps
                    // following the pointer and always ends.
                    let moved = entity.clone();
                    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _window, cx| {
                        if phase == DispatchPhase::Bubble {
                            moved.update(cx, |this, cx| this.on_canvas_move(ev, cx));
                        }
                    });
                    let released = entity.clone();
                    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _window, cx| {
                        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
                            released.update(cx, |this, cx| this.on_canvas_up(cx));
                        }
                    });
                }
                if bw <= 1.0 || bh <= 1.0 || iw <= 0.0 || ih <= 0.0 {
                    return;
                }
                // Fit, but never enlarge past the window's actual size.
                let scale = (bw / iw).min(bh / ih).min(1.0 / scale_factor);
                let (dw, dh) = (iw * scale, ih * scale);
                let origin_x = f32::from(bounds.origin.x) + (bw - dw) / 2.0;
                let origin_y = f32::from(bounds.origin.y) + (bh - dh) / 2.0;
                layout_cell.set(Some(CanvasLayout {
                    origin_x,
                    origin_y,
                    scale,
                }));
                let image_bounds =
                    Bounds::new(point(px(origin_x), px(origin_y)), size(px(dw), px(dh)));
                let _ = window.paint_image(
                    image_bounds,
                    image_bounds,
                    Default::default(),
                    render,
                    0,
                    excluded,
                );
                window.paint_quad(outline(image_bounds, Theme::border(), BorderStyle::Solid));
                if excluded {
                    return;
                }
                let map = |p: Pt| point(px(origin_x + p.x * scale), px(origin_y + p.y * scale));
                let arrow_color = gpui::rgb(0xE52B2B);
                // A large capture is shown heavily scaled down (a 4K window
                // fits at ~0.3×), which would shrink the badge digits below a
                // pixel. Keep arrows and badges at a readable on-screen size;
                // the uploaded image keeps its own burned-in sizes.
                let badge_width = preview_badge_width(arrow_width, scale);
                let arrow_width = preview_arrow_width(arrow_width, scale);
                // Arrows first, blackouts on top — the same order `flatten`
                // burns them in.
                for item in &items {
                    if let Annotation::Arrow { from, to } = item
                        && let Some(g) = annotate::arrow_geometry(*from, *to, arrow_width)
                    {
                        let mut shaft = gpui::PathBuilder::stroke(px(arrow_width * scale));
                        shaft.move_to(map(g.shaft.0));
                        shaft.line_to(map(g.shaft.1));
                        if let Ok(path) = shaft.build() {
                            window.paint_path(path, arrow_color);
                        }
                        let mut head = gpui::PathBuilder::fill();
                        head.move_to(map(g.head[0]));
                        head.line_to(map(g.head[1]));
                        head.line_to(map(g.head[2]));
                        head.close();
                        if let Ok(path) = head.build() {
                            window.paint_path(path, arrow_color);
                        }
                    }
                }
                // Numbered badges at each arrow's tail, over the shafts.
                for (item, number) in items.iter().zip(annotate::arrow_numbers(&items)) {
                    let (Annotation::Arrow { from, .. }, Some(number)) = (item, number) else {
                        continue;
                    };
                    let badge = annotate::badge_geometry(*from, number, badge_width, iw, ih);
                    let r = badge.radius * scale;
                    let c = map(badge.center);
                    window.paint_quad(gpui::quad(
                        Bounds::new(
                            point(c.x - px(r), c.y - px(r)),
                            size(px(r * 2.0), px(r * 2.0)),
                        ),
                        px(r),
                        arrow_color,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::Solid,
                    ));
                    for line in &badge.strokes {
                        let mut ink = gpui::PathBuilder::stroke(px(badge.stroke_width * scale));
                        for (i, p) in line.iter().enumerate() {
                            if i == 0 {
                                ink.move_to(map(*p));
                            } else {
                                ink.line_to(map(*p));
                            }
                        }
                        if let Ok(path) = ink.build() {
                            window.paint_path(path, gpui::white());
                        }
                    }
                }
                let rect_bounds = |r: &annotate::Rect| {
                    Bounds::new(
                        map(Pt::new(r.x, r.y)),
                        size(px(r.w * scale), px(r.h * scale)),
                    )
                };
                for item in &items {
                    if let Annotation::Blackout(r) = item {
                        window.paint_quad(fill(rect_bounds(r), gpui::black()));
                    }
                }
                // Selection chrome.
                let Some(item) = selected.and_then(|i| items.get(i)) else {
                    return;
                };
                let handle_quad = |center: gpui::Point<Pixels>| {
                    let half = px(HANDLE_PX / 2.0);
                    let b = Bounds::new(
                        point(center.x - half, center.y - half),
                        size(px(HANDLE_PX), px(HANDLE_PX)),
                    );
                    gpui::quad(
                        b,
                        px(2.0),
                        gpui::white(),
                        px(1.5),
                        Theme::accent(),
                        BorderStyle::Solid,
                    )
                };
                match item {
                    Annotation::Blackout(r) => {
                        let mut sel = outline(rect_bounds(r), Theme::accent(), BorderStyle::Solid);
                        sel.border_widths = px(1.5).into();
                        window.paint_quad(sel);
                        for handle in Handle::ALL {
                            window.paint_quad(handle_quad(map(annotate::handle_center(r, handle))));
                        }
                    }
                    Annotation::Arrow { from, to } => {
                        window.paint_quad(handle_quad(map(*from)));
                        window.paint_quad(handle_quad(map(*to)));
                    }
                }
            },
        )
        .size_full();

        let cursor = match (self.annotating(), self.tool) {
            (false, _) => CursorStyle::Arrow,
            (true, Tool::Select) => CursorStyle::Arrow,
            (true, _) => CursorStyle::Crosshair,
        };
        div()
            .id("general-feedback-canvas")
            .flex_1()
            .min_h(px(s(160.0)))
            .relative()
            .rounded_md()
            .border_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg())
            .overflow_hidden()
            .cursor(cursor)
            .track_focus(&self.canvas_focus)
            .on_key_down(cx.listener(Self::on_canvas_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_canvas_down))
            .child(painter)
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let small = px(Theme::font_size_ratio(0.85));
        let annotating = self.annotating();
        let tool_button = |id: &'static str, label: String, tool: Tool, tip: String| {
            Button::new(id)
                .label(label)
                .small()
                .selected(annotating && self.tool == tool)
                .disabled(!annotating)
                .tooltip(tip)
                .on_click(cx.listener(move |this, _, _, cx| this.set_tool(tool, cx)))
        };
        let has_selection = self.doc.selected().is_some();
        let include = self.include_screenshot;
        let entity = cx.entity();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(tool_button(
                "gfb-tool-select",
                crate::localization::text("feedback.general.select_tool"),
                Tool::Select,
                crate::localization::text("feedback.general.select_tool_tooltip"),
            ))
            .child(tool_button(
                "gfb-tool-arrow",
                crate::localization::text("feedback.general.arrow_tool"),
                Tool::Arrow,
                crate::localization::text("feedback.general.arrow_tool_tooltip"),
            ))
            .child(tool_button(
                "gfb-tool-blackout",
                crate::localization::text("feedback.general.blackout_tool"),
                Tool::Blackout,
                crate::localization::text("feedback.general.blackout_tool_tooltip"),
            ))
            .child(div().w(px(Theme::scaled(8.0))))
            .child(
                Button::new("gfb-undo")
                    .label(crate::localization::text("feedback.general.undo_button"))
                    .small()
                    .ghost()
                    .disabled(!annotating || !self.doc.can_undo())
                    .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
            )
            .child(
                Button::new("gfb-redo")
                    .label(crate::localization::text("feedback.general.redo_button"))
                    .small()
                    .ghost()
                    .disabled(!annotating || !self.doc.can_redo())
                    .on_click(cx.listener(|this, _, _, cx| this.redo(cx))),
            )
            .child(
                Button::new("gfb-delete")
                    .label(crate::localization::text("feedback.general.delete_button"))
                    .small()
                    .ghost()
                    .disabled(!annotating || !has_selection)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
            )
            .child(div().flex_1())
            .children(self.screenshot.as_ref().map(|_| {
                Checkbox::new("gfb-include-screenshot")
                    .checked(include)
                    .child(div().text_size(small).child(crate::localization::text(
                        "feedback.general.include_screenshot",
                    )))
                    .on_click(move |checked: &bool, _window, app| {
                        let checked = *checked;
                        entity.update(app, |this, cx| {
                            this.include_screenshot = checked;
                            this.drag = None;
                            cx.notify();
                        });
                    })
            }))
            .into_any_element()
    }
}

impl Render for GeneralFeedbackDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_input(window, cx);
        let small = px(Theme::font_size_ratio(0.85));

        let hint_key = if self.screenshot.is_none() {
            "feedback.general.hint_without_screenshot"
        } else if !self.include_screenshot {
            "feedback.general.hint_screenshot_excluded"
        } else {
            match self.tool {
                Tool::Select => "feedback.general.hint_select_tool",
                Tool::Arrow => "feedback.general.hint_arrow_tool",
                Tool::Blackout => "feedback.general.hint_blackout_tool",
            }
        };
        let hint = crate::localization::text(hint_key);

        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .text_color(Theme::text())
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .text_size(small)
                    .text_color(Theme::text_dim())
                    .child(hint),
            )
            .child(self.render_canvas(cx))
            .child(
                div()
                    .text_size(small)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(crate::localization::text("feedback.general.message_label")),
            )
            .children(self.input.as_ref().map(Textarea::new))
            .child(
                div()
                    .text_size(small)
                    .text_color(Theme::text_dim())
                    .child(crate::settings_snapshot::DISCLOSURE),
            )
            .children(self.error.as_ref().map(|error| {
                div()
                    .text_size(small)
                    .text_color(Theme::error())
                    .child(error.clone())
            }));

        let send = Button::new("gfb-send")
            .label(crate::localization::text("feedback.general.send_button"))
            .primary()
            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)));
        let cancel = Button::new("gfb-cancel")
            .label(crate::localization::text("feedback.general.cancel_button"))
            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx)));

        Dialog::new("general-feedback-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("feedback.general.dialog_title"))
            .bare_body()
            .windowed(true)
            .dismiss_on_scrim_click(false)
            .on_close(|this, window, cx| this.cancel(window, cx))
            .child(body)
            .button(cancel)
            .button(send)
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4K capture shown at ~0.3× still paints legible arrow numbers: the
    /// badge disc and its digit strokes keep a readable on-screen size, while
    /// a capture shown near full size keeps its burned-in width.
    #[test]
    fn preview_badges_stay_legible_when_the_capture_is_scaled_down() {
        let burned = crate::feedback_annotate::arrow_width_for_scale(1.0);
        let scale = 1120.0 / 3840.0;
        let width = preview_badge_width(burned, scale);
        let badge = crate::feedback_annotate::badge_geometry(
            crate::feedback_annotate::Pt::new(500.0, 500.0),
            7,
            width,
            3840.0,
            2160.0,
        );
        assert!(
            badge.radius * 2.0 * scale >= 20.0,
            "disc {}",
            badge.radius * 2.0 * scale
        );
        assert!(
            badge.stroke_width * scale >= 1.5,
            "ink {}",
            badge.stroke_width * scale
        );
        assert_eq!(preview_arrow_width(burned, 1.0), burned);
        // Badges are 3× their burned-in size in the preview.
        assert_eq!(preview_badge_width(burned, 1.0), burned * 3.0);
    }
    use std::cell::RefCell;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn shot() -> CapturedScreenshot {
        CapturedScreenshot::new(crate::screenshot::Screenshot {
            image: image::RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 255])),
            scale_factor: 2.0,
        })
    }

    #[test]
    fn captured_screenshot_keeps_rgba_and_swaps_render_copy() {
        let captured = shot();
        assert_eq!(captured.rgba.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(captured.size(), (64.0, 48.0));
        assert_eq!(captured.scale_factor, 2.0);
    }

    #[gpui::test]
    fn send_requires_a_message_then_emits_the_annotated_capture(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            GeneralFeedbackDialog::new(Ok(shot()), FeedbackContext::default(), cx)
        });
        let submitted: Rc<RefCell<Vec<SubmittedFeedback>>> = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |_, cx| {
            let entity = cx.entity();
            let log = submitted.clone();
            cx.subscribe(&entity, move |_, _, event: &GeneralFeedbackEvent, _| {
                let GeneralFeedbackEvent::Submitted(s) = event;
                log.borrow_mut().push(SubmittedFeedback {
                    message: s.message.clone(),
                    screenshot: s.screenshot.clone(),
                    annotations: s.annotations.clone(),
                    context: s.context.clone(),
                });
            })
            .detach();
        });
        view.update_in(cx, |dialog, window, cx| {
            dialog.ensure_input(window, cx);
            dialog.doc.add(Annotation::Blackout(annotate::Rect::new(
                1.0, 1.0, 10.0, 10.0,
            )));
            // Without the screenshot, an empty message has nothing to send.
            dialog.include_screenshot = false;
            dialog.submit(window, cx);
            assert!(dialog.error.is_some(), "an empty message must be rejected");
            dialog.include_screenshot = true;
            dialog.input.as_ref().unwrap().update(cx, |state, cx| {
                state.set_value("the results grid froze", window, cx)
            });
            dialog.submit(window, cx);
        });
        cx.run_until_parked();
        let log = submitted.borrow();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].message, "the results grid froze");
        assert_eq!(log[0].annotations.len(), 1);
        assert!(log[0].screenshot.is_some());
    }

    /// Regression: the production dialog is owned only by its window, and
    /// Send closes that window. Closing it in the same update dropped the
    /// dialog before GPUI delivered `Submitted`, so the report vanished.
    #[gpui::test]
    fn send_reaches_the_host_even_though_it_closes_the_dialog_window(
        cx: &mut gpui::TestAppContext,
    ) {
        ensure_components_initialized(cx);
        let got: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let dialog_window = cx.add_window(|_window, cx| {
            GeneralFeedbackDialog::new(Ok(shot()), FeedbackContext::default(), cx)
        });
        let dialog = dialog_window.root(cx).unwrap();
        // The host is a different entity holding only a subscription, like
        // AppView.
        let host = cx.new(|_| ());
        host.update(cx, |_, cx| {
            let got = got.clone();
            cx.subscribe(&dialog, move |_, _, event: &GeneralFeedbackEvent, _| {
                let GeneralFeedbackEvent::Submitted(s) = event;
                got.borrow_mut().push(s.message.clone());
            })
            .detach();
        });
        drop(dialog);
        dialog_window
            .update(cx, |dialog, window, cx| {
                dialog.ensure_input(window, cx);
                dialog.input.as_ref().unwrap().update(cx, |state, cx| {
                    state.set_value("the grid froze", window, cx)
                });
                dialog.submit(window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(*got.borrow(), vec!["the grid froze".to_string()]);
        assert!(
            dialog_window.update(cx, |_, _, _| ()).is_err(),
            "the dialog window closes after sending"
        );
        drop(host);
    }

    #[gpui::test]
    fn unticking_include_screenshot_sends_the_message_alone(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            GeneralFeedbackDialog::new(Ok(shot()), FeedbackContext::default(), cx)
        });
        let got: Rc<RefCell<Option<(bool, usize)>>> = Rc::new(RefCell::new(None));
        view.update(cx, |_, cx| {
            let entity = cx.entity();
            let got = got.clone();
            cx.subscribe(&entity, move |_, _, event: &GeneralFeedbackEvent, _| {
                let GeneralFeedbackEvent::Submitted(s) = event;
                *got.borrow_mut() = Some((s.screenshot.is_some(), s.annotations.len()));
            })
            .detach();
        });
        view.update_in(cx, |dialog, window, cx| {
            dialog.ensure_input(window, cx);
            dialog.doc.add(Annotation::Arrow {
                from: Pt::new(1.0, 1.0),
                to: Pt::new(30.0, 30.0),
            });
            dialog.include_screenshot = false;
            assert!(!dialog.annotating());
            dialog.input.as_ref().unwrap().update(cx, |state, cx| {
                state.set_value("idea: dark mode for charts", window, cx)
            });
            dialog.submit(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(*got.borrow(), Some((false, 0)));
    }

    #[gpui::test]
    fn missing_capture_explains_and_disables_annotation(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            GeneralFeedbackDialog::new(
                Err("screenshots are not supported on this platform yet".into()),
                FeedbackContext::default(),
                cx,
            )
        });
        view.update(cx, |dialog, _| {
            assert!(!dialog.annotating());
            assert!(!dialog.include_screenshot);
            assert!(dialog.unavailable.is_some());
        });
    }
}
