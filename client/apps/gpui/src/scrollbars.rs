//! Click-and-drag support for the app's custom-painted scrollbars.
//!
//! Several surfaces (SQL editor, preferences, connection management, entity
//! management, connection editor, IntelliSense feedback preview) paint their
//! own track + thumb instead of using gpui-component's `Scrollbar`. This
//! module gives them one shared interaction model: pressing anywhere on a
//! track jumps to that position, and holding the button drags the thumb.
//!
//! The drag target lives in a `Global` so the *view-agnostic* container
//! listeners (`scrollbar_drag_area`) can keep following the pointer after it
//! wanders off the narrow track mid-drag — the same trick the schema tree
//! plays with its `scrollbar_dragging` bool, minus the per-view field.

use gpui::{
    App, Axis, Global, InteractiveElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, point, px,
};

/// The scrollbar currently being dragged, if any. One drag can be live at a
/// time — a mouse can only hold one thumb.
#[derive(Default)]
struct ScrollbarDrag {
    target: Option<(ScrollHandle, Axis)>,
}

impl Global for ScrollbarDrag {}

/// Map a window-space pointer position onto `handle`'s scroll range along
/// `axis` — the position within the scroll container's bounds becomes the
/// proportional scroll offset (the schema tree's `scroll_to_pointer`,
/// generalized). Custom tracks span their scroll container on the drag axis,
/// so the container's bounds double as the track geometry.
pub fn scroll_to_pointer(handle: &ScrollHandle, axis: Axis, position: Point<Pixels>) {
    let bounds = handle.bounds();
    let offset = handle.offset();
    match axis {
        Axis::Vertical => {
            if bounds.size.height <= px(0.0) {
                return;
            }
            let ratio = ((position.y - bounds.origin.y) / bounds.size.height).clamp(0.0, 1.0);
            handle.set_offset(point(offset.x, -(handle.max_offset().y * ratio)));
        }
        Axis::Horizontal => {
            if bounds.size.width <= px(0.0) {
                return;
            }
            let ratio = ((position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
            handle.set_offset(point(-(handle.max_offset().x * ratio), offset.y));
        }
    }
}

/// Jump to the pressed position and begin dragging `handle`'s thumb.
pub fn begin_drag(cx: &mut App, handle: &ScrollHandle, axis: Axis, position: Point<Pixels>) {
    scroll_to_pointer(handle, axis, position);
    cx.default_global::<ScrollbarDrag>().target = Some((handle.clone(), axis));
}

/// Follow the pointer while a drag is live. Returns whether a drag consumed
/// the move (callers repaint on `true`).
fn update_drag(cx: &mut App, position: Point<Pixels>) -> bool {
    match &cx.default_global::<ScrollbarDrag>().target {
        Some((handle, axis)) => {
            scroll_to_pointer(handle, *axis, position);
            true
        }
        None => false,
    }
}

/// End any live drag. Returns whether one was live.
fn end_drag(cx: &mut App) -> bool {
    cx.default_global::<ScrollbarDrag>().target.take().is_some()
}

/// Fluent wiring for elements participating in scrollbar dragging.
pub trait DraggableScrollbar: InteractiveElement + Sized {
    /// Make this element a scrollbar track (or thumb) for `handle` along
    /// `axis`: pressing jumps there and starts a drag.
    fn scrollbar_track(self, handle: &ScrollHandle, axis: Axis) -> Self {
        let handle = handle.clone();
        self.on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
            begin_drag(cx, &handle, axis, ev.position);
            cx.stop_propagation();
            window.refresh();
        })
    }

    /// Track a live scrollbar drag across this (larger) container so the
    /// pointer can wander off the narrow track without the thumb stopping
    /// dead. Attach to the root of any surface that renders a
    /// `scrollbar_track`.
    fn scrollbar_drag_area(self) -> Self {
        self.on_mouse_move(|ev: &MouseMoveEvent, window, cx| {
            if update_drag(cx, ev.position) {
                window.refresh();
            }
        })
        .on_mouse_up(MouseButton::Left, |_: &MouseUpEvent, window, cx| {
            if end_drag(cx) {
                window.refresh();
            }
        })
        // Release outside the container (or the window) must not leave a
        // sticky drag behind.
        .on_mouse_up_out(MouseButton::Left, |_: &MouseUpEvent, window, cx| {
            if end_drag(cx) {
                window.refresh();
            }
        })
    }
}

impl<E: InteractiveElement> DraggableScrollbar for E {}
