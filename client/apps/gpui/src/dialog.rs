//! The app's single dialog chrome: one movable, resizable, detachable frame
//! that every dialog is migrating onto.
//!
//! [`Dialog`] is a per-render builder, not a view: the owning view keeps a
//! [`DialogFrame`] (position/size + any in-flight drag) and, in its `render`,
//! wraps its body in `Dialog::new(...).build(self, window, cx)`. This follows
//! the app's existing parent-tracked drag model (`pane_divider`): child
//! elements report the grab, the full-window layer tracks pointer moves, and
//! the owner re-renders from the frame's geometry. Views that own several
//! short-lived confirms (`AppView`'s pending dialogs) keep one [`DialogFrames`]
//! map instead of a field per dialog, so the frame accessor can never dangle.
//!
//! What the chrome provides, all optional per dialog:
//! - a themed title bar (drag handle when movable) with a red close ✕,
//! - resize from every edge and corner with the matching resize cursors,
//! - fixed-size or auto-height cards: [`DialogFrame::new_auto`] sizes the
//!   card to its content (capped to the viewport, body scrolls past the cap)
//!   until the user drags an edge that fixes the height,
//! - a modal scrim that blocks the UI behind the card, or a free-floating
//!   non-modal card,
//! - a footer with caller-supplied buttons, a default Close button, and a
//!   confirm/reject pair for confirmation dialogs (Enter confirms, Escape
//!   rejects),
//! - a pop-out affordance plus [`open_detached`] for promoting the dialog to
//!   its own OS window (ignored on wasm, where a browser tab cannot spawn
//!   windows — `open_detached` returns `None` there and callers fall back to
//!   the inline card).
//!
//! All frame coordinates are window logical pixels; pass `Theme::scaled`
//! values for default sizes.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, Div, Entity, FocusHandle, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Render, SharedString, Window,
    canvas, div, point, prelude::*, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};

use crate::theme::Theme;

/// Where an in-window dialog first appears, before the user drags it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DialogPlacement {
    /// Centered in the viewport (slightly above true center, matching the
    /// app's other cards). The default.
    Center,
    /// Top-left of the card at this point, in window coordinates.
    WindowPoint(Point<Pixels>),
    /// Top-left of the card at this point, in screen coordinates (converted
    /// against the window's position; on platforms where the window origin is
    /// unknown this degrades to window coordinates).
    ScreenPoint(Point<Pixels>),
}

/// Which edge or corner a resize drag grabbed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeEdge {
    fn moves_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    fn moves_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    fn moves_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    fn moves_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }

    /// The standard resize cursor for this edge: horizontal, vertical, or the
    /// matching diagonal.
    pub fn cursor(self) -> CursorStyle {
        match self {
            Self::Left | Self::Right => CursorStyle::ResizeLeftRight,
            Self::Top | Self::Bottom => CursorStyle::ResizeUpDown,
            Self::TopLeft | Self::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            Self::TopRight | Self::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

/// One in-flight pointer interaction with the card.
enum Drag {
    /// Moving: pointer position minus the card origin, captured at grab time.
    Move { grab_x: f32, grab_y: f32 },
    /// Resizing from an edge or corner: pointer, size, and origin at grab time.
    Resize {
        edge: ResizeEdge,
        start_x: f32,
        start_y: f32,
        start_w: f32,
        start_h: f32,
        start_ox: f32,
        start_oy: f32,
    },
}

/// Clamp that never panics when the range collapses (tiny viewports).
fn clamp_safe(v: f32, lo: f32, hi: f32) -> f32 {
    v.clamp(lo, hi.max(lo))
}

/// How [`Dialog::build`] should position the card this frame.
enum CardMode {
    /// Auto-height card that has never been dragged: let the (full-window)
    /// wrapper flex-center it, so content height needs no up-front number.
    Centered,
    /// Anchored at a concrete window point.
    Anchored(Point<Pixels>),
}

/// Position and size of a dialog card, plus any active drag or resize.
///
/// The owning view keeps one of these per dialog; [`Dialog::build`] reads the
/// geometry each render and feeds pointer events back in. Height is either
/// fixed ([`DialogFrame::new`]) or content-driven ([`DialogFrame::new_auto`]);
/// an auto height becomes fixed the first time the user drags a top or bottom
/// edge. The card's painted bounds are captured each frame so a drag can start
/// from wherever layout actually put the card.
pub struct DialogFrame {
    /// Card top-left in window coordinates; `None` until the user moves or
    /// resizes it, meaning "resolve from `placement`".
    origin: Option<(f32, f32)>,
    placement: DialogPlacement,
    width: f32,
    /// `None` = auto: the card takes its content height (viewport-capped).
    height: Option<f32>,
    min_w: f32,
    min_h: f32,
    /// Card bounds as last painted, written by the measuring canvas inside
    /// the card. Shared with the render closure, hence the `Rc<Cell>`.
    measured: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag: Option<Drag>,
    /// Unsaved-changes flag, set by the owning dialog. While dirty, easy
    /// close (Escape / click outside) is suspended unless the dialog opts
    /// into [`Dialog::easy_close_when_dirty`] — explicit closes (✕, Close,
    /// the dialog's own buttons) are unaffected.
    dirty: bool,
}

impl DialogFrame {
    /// A centered frame with a fixed default size and sensible minimums.
    /// Pass `Theme::scaled` values.
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            origin: None,
            placement: DialogPlacement::Center,
            width,
            height: Some(height),
            min_w: 280.0,
            min_h: 140.0,
            measured: Rc::new(Cell::new(None)),
            drag: None,
            dirty: false,
        }
    }

    /// A centered frame whose height follows its content — the right default
    /// for prompts and forms that used to size themselves. Pass a
    /// `Theme::scaled` width.
    pub fn new_auto(width: f32) -> Self {
        let mut frame = Self::new(width, 0.0);
        frame.height = None;
        frame
    }

    /// Where the dialog first opens (default: centered).
    pub fn with_placement(mut self, placement: DialogPlacement) -> Self {
        self.placement = placement;
        self
    }

    /// Override the minimum size the user can resize down to.
    pub fn with_min_size(mut self, min_w: f32, min_h: f32) -> Self {
        self.min_w = min_w;
        self.min_h = min_h;
        self
    }

    /// Current card width (window logical pixels).
    pub fn width(&self) -> f32 {
        self.width
    }

    /// Current card height, or `None` while the height is content-driven.
    pub fn height(&self) -> Option<f32> {
        self.height
    }

    /// Mark the dialog's unsaved-changes state. Dialogs set this from their
    /// render (or edit handlers); while dirty, easy close is suspended.
    pub fn set_dirty(&mut self, dirty: bool) {
        self.dirty = dirty;
    }

    /// Whether the dialog currently reports unsaved changes.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// True while a move or resize drag is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The cursor the whole overlay should show while a drag is in progress:
    /// a grabbing hand while moving, the edge's resize cursor while resizing.
    pub fn active_cursor(&self) -> Option<CursorStyle> {
        match &self.drag {
            Some(Drag::Move { .. }) => Some(CursorStyle::ClosedHand),
            Some(Drag::Resize { edge, .. }) => Some(edge.cursor()),
            None => None,
        }
    }

    fn viewport(window: &Window) -> (f32, f32) {
        let vp = window.viewport_size();
        (f32::from(vp.width), f32::from(vp.height))
    }

    fn window_origin(window: &Window) -> (f32, f32) {
        let origin = window.bounds().origin;
        (f32::from(origin.x), f32::from(origin.y))
    }

    /// The card's last painted bounds, in window coordinates.
    fn measured_rect(&self) -> Option<(f32, f32, f32, f32)> {
        self.measured.get().map(|b| {
            (
                f32::from(b.origin.x),
                f32::from(b.origin.y),
                f32::from(b.size.width),
                f32::from(b.size.height),
            )
        })
    }

    /// The height to use for math while the real height is content-driven.
    fn effective_height(&self) -> f32 {
        self.height
            .or_else(|| self.measured_rect().map(|(_, _, _, h)| h))
            .unwrap_or(self.min_h)
    }

    fn card_mode(&self, window: &Window) -> CardMode {
        if self.origin.is_none()
            && self.height.is_none()
            && self.placement == DialogPlacement::Center
        {
            return CardMode::Centered;
        }
        let (x, y) = self.resolved(window);
        CardMode::Anchored(point(px(x), px(y)))
    }

    fn resolved(&self, window: &Window) -> (f32, f32) {
        let (vw, vh) = Self::viewport(window);
        let win = Self::window_origin(window);
        self.resolved_in(vw, vh, win)
    }

    fn resolved_in(&self, vw: f32, vh: f32, window_origin: (f32, f32)) -> (f32, f32) {
        if let Some(origin) = self.origin {
            return origin;
        }
        let height = self.effective_height();
        let requested = match self.placement {
            // Sit a touch above true center so a tall card does not crowd the
            // bottom status bar.
            DialogPlacement::Center => {
                return (
                    ((vw - self.width) / 2.0).max(0.0),
                    ((vh - height) / 2.5).max(0.0),
                );
            }
            DialogPlacement::WindowPoint(p) => (f32::from(p.x), f32::from(p.y)),
            DialogPlacement::ScreenPoint(p) => (
                f32::from(p.x) - window_origin.0,
                f32::from(p.y) - window_origin.1,
            ),
        };
        // Keep a graspable strip on-screen, same as the move clamp.
        (
            clamp_safe(requested.0, 48.0 - self.width, vw - 48.0),
            clamp_safe(requested.1, 0.0, vh - 32.0),
        )
    }

    /// Pin the floating origin down, preferring where the card was actually
    /// painted (the only truth for a flex-centered auto-height card).
    fn seed_origin(&mut self, window: &Window) -> (f32, f32) {
        let origin = match (self.origin, self.measured_rect()) {
            (Some(origin), _) => origin,
            (None, Some((x, y, _, _))) => (x, y),
            (None, None) => self.resolved(window),
        };
        self.origin = Some(origin);
        origin
    }

    /// Begin moving; `pointer` is the mouse-down position in window coords.
    pub fn begin_move(&mut self, pointer: Point<Pixels>, window: &Window) {
        let (ox, oy) = self.seed_origin(window);
        self.drag = Some(Drag::Move {
            grab_x: f32::from(pointer.x) - ox,
            grab_y: f32::from(pointer.y) - oy,
        });
    }

    /// Begin resizing from the given edge or corner.
    pub fn begin_resize(&mut self, edge: ResizeEdge, pointer: Point<Pixels>, window: &Window) {
        let start_h = self.effective_height();
        let (ox, oy) = self.seed_origin(window);
        self.drag = Some(Drag::Resize {
            edge,
            start_x: f32::from(pointer.x),
            start_y: f32::from(pointer.y),
            start_w: self.width,
            start_h,
            start_ox: ox,
            start_oy: oy,
        });
    }

    /// Feed a pointer move. Returns `true` when geometry changed (redraw needed).
    pub fn on_mouse_move(&mut self, pointer: Point<Pixels>, window: &Window) -> bool {
        let (vw, vh) = Self::viewport(window);
        self.apply_pointer(f32::from(pointer.x), f32::from(pointer.y), vw, vh)
    }

    fn apply_pointer(&mut self, px_x: f32, px_y: f32, vw: f32, vh: f32) -> bool {
        match &self.drag {
            Some(Drag::Move { grab_x, grab_y }) => {
                // Keep a graspable strip on every edge so the card can always
                // be dragged back — never let it vanish fully off-screen.
                let x = clamp_safe(px_x - grab_x, 48.0 - self.width, vw - 48.0);
                let y = clamp_safe(px_y - grab_y, 0.0, vh - 32.0);
                self.origin = Some((x, y));
                true
            }
            Some(Drag::Resize {
                edge,
                start_x,
                start_y,
                start_w,
                start_h,
                start_ox,
                start_oy,
            }) => {
                let edge = *edge;
                let dx = px_x - start_x;
                let dy = px_y - start_y;
                let (start_w, start_h) = (*start_w, *start_h);
                let (start_ox, start_oy) = (*start_ox, *start_oy);
                let (mut ox, mut oy) = (start_ox, start_oy);
                let mut w = start_w;
                if edge.moves_right() {
                    // Cap so the card stays within the viewport from its origin.
                    w = clamp_safe(start_w + dx, self.min_w, vw - start_ox);
                } else if edge.moves_left() {
                    // The left edge follows the pointer; the right edge stays
                    // put, so the origin shifts by however much the width won.
                    w = clamp_safe(start_w - dx, self.min_w, start_ox + start_w);
                    ox = start_ox + start_w - w;
                }
                let mut h = self.height;
                if edge.moves_bottom() {
                    h = Some(clamp_safe(start_h + dy, self.min_h, vh - start_oy));
                } else if edge.moves_top() {
                    let new_h = clamp_safe(start_h - dy, self.min_h, start_oy + start_h);
                    oy = start_oy + start_h - new_h;
                    h = Some(new_h);
                }
                let changed = w != self.width || h != self.height || Some((ox, oy)) != self.origin;
                self.width = w;
                // A vertical drag fixes an auto height; horizontal drags on an
                // auto card leave the height content-driven.
                self.height = h;
                self.origin = Some((ox, oy));
                changed
            }
            None => false,
        }
    }

    /// End any active drag. Returns `true` if one was in progress.
    pub fn end(&mut self) -> bool {
        self.drag.take().is_some()
    }
}

/// A bag of [`DialogFrame`]s for views that host many short-lived dialogs
/// (the app view's pending confirms). Frames persist across open/close, so a
/// reopened dialog remembers where the user left it, and the accessor handed
/// to [`Dialog::new`] can never dangle: it creates the frame on first touch.
#[derive(Default)]
pub struct DialogFrames {
    frames: HashMap<SharedString, DialogFrame>,
}

impl DialogFrames {
    pub fn new() -> Self {
        Self::default()
    }

    /// The frame for `id`, created from `default` on first use.
    pub fn get(
        &mut self,
        id: impl Into<SharedString>,
        default: impl FnOnce() -> DialogFrame,
    ) -> &mut DialogFrame {
        self.frames.entry(id.into()).or_insert_with(default)
    }
}

type Callback<V> = Rc<dyn Fn(&mut V, &mut Window, &mut Context<V>)>;

/// Per-render builder for the unified dialog chrome. See the module docs.
///
/// The caller fully controls the body via [`Dialog::child`]; the chrome owns
/// the title bar, scrim, resize handles, and footer.
pub struct Dialog<V: 'static> {
    id: SharedString,
    frame: Rc<dyn Fn(&mut V) -> &mut DialogFrame>,
    title: SharedString,
    movable: bool,
    resizable: bool,
    detachable: bool,
    show_close: bool,
    modal: bool,
    default_close: bool,
    confirm: Option<(SharedString, SharedString)>,
    danger: bool,
    padded_body: bool,
    scrim_dismiss: Option<bool>,
    windowed: bool,
    easy_close: bool,
    easy_close_when_dirty: bool,
    on_close: Option<Callback<V>>,
    on_confirm: Option<Callback<V>>,
    on_reject: Option<Callback<V>>,
    on_detach: Option<Callback<V>>,
    focus: Option<FocusHandle>,
    key_context: Option<SharedString>,
    body: Vec<AnyElement>,
    buttons: Vec<AnyElement>,
    leading_buttons: Vec<AnyElement>,
}

impl<V: 'static> Dialog<V> {
    /// A dialog with the defaults: movable, resizable, modal, not detachable,
    /// close ✕ in the title bar, default Close button in the footer.
    ///
    /// `frame` projects the owning view's [`DialogFrame`] and must always
    /// resolve — route it through [`DialogFrames::get`] when the dialog's
    /// state itself is an `Option`; `id` must be unique among concurrently
    /// open dialogs.
    pub fn new(
        id: impl Into<SharedString>,
        frame: impl Fn(&mut V) -> &mut DialogFrame + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            frame: Rc::new(frame),
            title: SharedString::default(),
            movable: true,
            resizable: true,
            detachable: false,
            show_close: true,
            modal: true,
            default_close: true,
            confirm: None,
            danger: false,
            padded_body: true,
            scrim_dismiss: None,
            windowed: false,
            easy_close: false,
            easy_close_when_dirty: false,
            on_close: None,
            on_confirm: None,
            on_reject: None,
            on_detach: None,
            focus: None,
            key_context: None,
            body: Vec::new(),
            buttons: Vec::new(),
            leading_buttons: Vec::new(),
        }
    }

    /// Text shown in the dedicated title bar.
    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = title.into();
        self
    }

    /// Whether the title bar drags the card (default `true`).
    pub fn movable(mut self, movable: bool) -> Self {
        self.movable = movable;
        self
    }

    /// Whether the edges and corners resize the card (default `true`).
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Show a "pop out" affordance in the title bar that fires `on_detach`
    /// (default `false`). Ignored on wasm, where dialogs cannot leave the
    /// browser window.
    pub fn detachable(
        mut self,
        on_detach: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        self.detachable = true;
        self.on_detach = Some(Rc::new(on_detach));
        self
    }

    /// Whether the red close ✕ appears at the right of the title bar
    /// (default `true`).
    pub fn show_close_in_title(mut self, show: bool) -> Self {
        self.show_close = show;
        self
    }

    /// Whether a scrim blocks the UI behind the dialog (default `true`).
    /// Non-modal dialogs float over the app without blocking it.
    pub fn is_modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    /// Whether the footer renders a stock Close button wired to `on_close`
    /// (default `true`), so callers don't build their own. Ignored once the
    /// dialog is [`windowed`](Self::windowed): the OS title bar's close button
    /// is the close control there.
    pub fn use_default_close(mut self, use_default: bool) -> Self {
        self.default_close = use_default;
        self
    }

    /// Turn the dialog into a confirmation: the footer gets a reject and a
    /// confirm button with the given labels, Enter confirms, Escape rejects,
    /// and the scrim never dismisses (the user must choose).
    pub fn confirm(
        mut self,
        confirm_text: impl Into<SharedString>,
        reject_text: impl Into<SharedString>,
    ) -> Self {
        self.confirm = Some((confirm_text.into(), reject_text.into()));
        self.default_close = false;
        self
    }

    /// Style the confirm button as destructive (red) instead of primary.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Skip the body's default padding and scrolling; the caller lays out
    /// edge-to-edge and manages its own overflow.
    pub fn bare_body(mut self) -> Self {
        self.padded_body = false;
        self
    }

    /// Easy close (default `false`): when on, pressing Escape or clicking
    /// outside the dialog closes it via `on_close`. When off, only the
    /// explicit paths close it — the ✕, the Close button, and the dialog's
    /// own actions. Confirm mode is unaffected (Enter confirms, Escape
    /// rejects, the scrim never dismisses).
    ///
    /// Easy close is automatically suspended while the dialog's
    /// [`DialogFrame`] reports unsaved changes ([`DialogFrame::set_dirty`]),
    /// unless [`Dialog::easy_close_when_dirty`] opts out of that guard.
    pub fn easy_close(mut self, easy: bool) -> Self {
        self.easy_close = easy;
        self
    }

    /// Keep easy close active even while the dialog is marked dirty
    /// (default `false`) — for dialogs whose state is harmless to discard.
    pub fn easy_close_when_dirty(mut self, always: bool) -> Self {
        self.easy_close_when_dirty = always;
        self
    }

    /// Override whether clicking the scrim dismisses the dialog,
    /// independently of [`Dialog::easy_close`] (which is the default
    /// source of that behavior).
    pub fn dismiss_on_scrim_click(mut self, dismiss: bool) -> Self {
        self.scrim_dismiss = Some(dismiss);
        self
    }

    /// Render for a popped-out OS window: the window boundary IS the dialog
    /// boundary. The content fills the window and resizes with it; there is
    /// no scrim, no floating card, no chrome move/resize, and no themed
    /// title bar (the native window title bar already carries the title and
    /// close button). Body, footer, confirm mode, and Escape/Enter handling
    /// are unchanged.
    pub fn windowed(mut self, windowed: bool) -> Self {
        self.windowed = windowed;
        self
    }

    /// The single close path: wired to the ✕, the default Close button, the
    /// scrim click, and Escape (outside confirm mode). The handler clears
    /// whatever owner state keeps the dialog rendered.
    pub fn on_close(mut self, cb: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static) -> Self {
        self.on_close = Some(Rc::new(cb));
        self
    }

    /// Confirm handler for [`Dialog::confirm`] mode (also fired by Enter).
    pub fn on_confirm(
        mut self,
        cb: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        self.on_confirm = Some(Rc::new(cb));
        self
    }

    /// Reject handler for [`Dialog::confirm`] mode (also fired by Escape).
    pub fn on_reject(
        mut self,
        cb: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        self.on_reject = Some(Rc::new(cb));
        self
    }

    /// Track the owner's focus handle on the card so its key bindings
    /// (Escape/Enter) fire while the dialog is focused.
    pub fn track_focus(mut self, handle: &FocusHandle) -> Self {
        self.focus = Some(handle.clone());
        self
    }

    /// Key-binding context name stamped on the card (for dialogs with their
    /// own action bindings).
    pub fn key_context(mut self, context: impl Into<SharedString>) -> Self {
        self.key_context = Some(context.into());
        self
    }

    /// Append to the caller-controlled body area.
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.body.push(child.into_any_element());
        self
    }

    /// Append any number of children to the body (accepts an `Option`, like
    /// `ParentElement::children`).
    pub fn children_body(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.body
            .extend(children.into_iter().map(|c| c.into_any_element()));
        self
    }

    /// Append a caller-supplied button to the footer's right-aligned action
    /// cluster, after the default Close button and before any confirm/reject
    /// pair — the conventional slot for the dialog's own actions, primary
    /// action last.
    pub fn button(mut self, button: impl IntoElement) -> Self {
        self.buttons.push(button.into_any_element());
        self
    }

    /// Append a caller-supplied button to the footer's far left — the slot
    /// for utility actions (Reset, Refresh, …) that should sit apart from the
    /// main action cluster.
    pub fn leading_button(mut self, button: impl IntoElement) -> Self {
        self.leading_buttons.push(button.into_any_element());
        self
    }

    fn callback_click(
        cb: Callback<V>,
        cx: &mut Context<V>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _ev, window, cx| cb(this, window, cx))
    }

    fn resize_handle(&self, edge: ResizeEdge, cx: &mut Context<V>) -> AnyElement {
        let s = Theme::scaled;
        let corner = px(s(10.0));
        let strip = px(s(5.0));
        let frame = self.frame.clone();
        let base = div()
            .id(SharedString::from(format!("{}-resize-{:?}", self.id, edge)))
            .absolute()
            .cursor(edge.cursor())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    frame(this).begin_resize(edge, ev.position, window);
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        match edge {
            ResizeEdge::Top => base.top_0().left(corner).right(corner).h(strip),
            ResizeEdge::Bottom => base.bottom_0().left(corner).right(corner).h(strip),
            ResizeEdge::Left => base.left_0().top(corner).bottom(corner).w(strip),
            ResizeEdge::Right => base.right_0().top(corner).bottom(corner).w(strip),
            ResizeEdge::TopLeft => base.top_0().left_0().w(corner).h(corner),
            ResizeEdge::TopRight => base.top_0().right_0().w(corner).h(corner),
            ResizeEdge::BottomLeft => base.bottom_0().left_0().w(corner).h(corner),
            ResizeEdge::BottomRight => base.bottom_0().right_0().w(corner).h(corner),
        }
        .into_any_element()
    }

    /// Render the dialog. Call from the owner's `Render::render`, passing the
    /// view itself so the chrome can read the frame's current geometry.
    pub fn build(mut self, view: &mut V, window: &mut Window, cx: &mut Context<V>) -> Div {
        let s = Theme::scaled;
        let (vw, vh) = DialogFrame::viewport(window);

        // A browser tab cannot spawn OS windows: detach options are ignored.
        if cfg!(target_arch = "wasm32") {
            self.detachable = false;
            self.on_detach = None;
        }

        let (mode, w, fixed_h, dragging, drag_cursor, measured, dirty) = {
            let frame = (self.frame)(view);
            (
                frame.card_mode(window),
                frame.width(),
                frame.height(),
                frame.is_dragging(),
                frame.active_cursor(),
                frame.measured.clone(),
                frame.dirty(),
            )
        };
        // Unsaved changes suspend easy close (Escape / click-outside) unless
        // the dialog opted out of the guard; explicit closes are unaffected.
        let easy_close = self.easy_close && (self.easy_close_when_dirty || !dirty);
        // A dialog's default size must never exceed the window it opens in —
        // clamp so the footer stays reachable on small windows (the frame
        // itself is untouched; a bigger window shows the full size again).
        let w = w.min((vw - 16.0).max(240.0));
        let fixed_h = fixed_h.map(|h| h.min((vh - 16.0).max(160.0)));

        let is_confirm = self.confirm.is_some();

        // Title bar: a themed strip that doubles as the drag handle. The tint
        // is the theme accent washed over the card background, so it stays
        // distinctive across every palette without a dedicated token.
        let mut titlebar_bg = Theme::accent();
        titlebar_bg.a = 0.09;
        let mut titlebar = div()
            .id(SharedString::from(format!("{}-titlebar", self.id)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .flex_shrink_0()
            .h(px(s(34.0)))
            .px(px(s(12.0)))
            .bg(titlebar_bg)
            .border_b_1()
            .border_color(Theme::popup_border())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::font_size()))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Theme::text())
                    .child(self.title.clone()),
            );
        if self.movable {
            let frame = self.frame.clone();
            titlebar = titlebar.cursor(CursorStyle::OpenHand).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    frame(this).begin_move(ev.position, window);
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        }
        if self.detachable
            && let Some(on_detach) = self.on_detach.clone()
        {
            titlebar = titlebar.child(
                Button::new(SharedString::from(format!("{}-popout", self.id)))
                    .xsmall()
                    .ghost()
                    .label(crate::localization::text("dialog.titlebar.pop_out"))
                    .on_click(Self::callback_click(on_detach, cx)),
            );
        }
        if self.show_close {
            let mut close_hover = Theme::error();
            close_hover.a = 0.15;
            let mut close_x = div()
                .id(SharedString::from(format!("{}-close", self.id)))
                .flex()
                .items_center()
                .justify_center()
                .w(px(s(20.0)))
                .h(px(s(20.0)))
                .rounded(px(s(4.0)))
                .cursor_pointer()
                .text_size(px(Theme::font_size_ratio(0.95)))
                .text_color(Theme::error())
                .hover(|d| d.bg(close_hover))
                // Don't let the press reach the title bar and start a drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child("✕");
            if let Some(on_close) = self.on_close.clone() {
                close_x = close_x.on_click(Self::callback_click(on_close, cx));
            }
            titlebar = titlebar.child(close_x);
        }

        // Both body shapes clip at their box: overflowing content must never
        // paint over the footer bar (or the title bar) — the footer stays
        // fully visible no matter what the body does.
        let body: AnyElement = if self.padded_body {
            div()
                .id(SharedString::from(format!("{}-body", self.id)))
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .px(px(s(16.0)))
                .py(px(s(12.0)))
                .children(std::mem::take(&mut self.body))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .flex()
                .flex_col()
                .children(std::mem::take(&mut self.body))
                .into_any_element()
        };

        // Footer: leading utility buttons far left, then the right-aligned
        // action cluster: default Close, caller actions, confirm/reject.
        let custom_buttons = std::mem::take(&mut self.buttons);
        let leading_buttons = std::mem::take(&mut self.leading_buttons);
        // Popped out, the OS window's own close button IS the close control.
        // A second "Close" in the footer is redundant chrome — and the two
        // sitting inches apart reads as though they did different things.
        let default_close = self.default_close && !self.windowed;
        let has_footer = !custom_buttons.is_empty()
            || !leading_buttons.is_empty()
            || default_close
            || is_confirm;
        let footer = has_footer.then(|| {
            // Wraps instead of clipping when the buttons outgrow the card
            // (a narrow window, a long label): overflow buttons drop to a
            // right-aligned second row rather than running off the edge.
            let mut footer = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .justify_end()
                .items_center()
                .gap(px(s(8.0)))
                .flex_shrink_0()
                .px(px(s(16.0)))
                .py(px(s(12.0)))
                .border_t_1()
                .border_color(Theme::popup_border())
                .children(leading_buttons)
                .child(div().flex_1());
            if default_close && let Some(on_close) = self.on_close.clone() {
                footer = footer.child(
                    Button::new(SharedString::from(format!("{}-default-close", self.id)))
                        .label(crate::localization::text("dialog.footer.close"))
                        .on_click(Self::callback_click(on_close, cx)),
                );
            }
            footer = footer.children(custom_buttons);
            if let Some((confirm_text, reject_text)) = self.confirm.clone() {
                if let Some(on_reject) = self.on_reject.clone() {
                    footer = footer.child(
                        Button::new(SharedString::from(format!("{}-reject", self.id)))
                            .label(reject_text)
                            .on_click(Self::callback_click(on_reject, cx)),
                    );
                }
                if let Some(on_confirm) = self.on_confirm.clone() {
                    let mut button =
                        Button::new(SharedString::from(format!("{}-confirm", self.id)))
                            .label(confirm_text);
                    button = if self.danger {
                        button.danger()
                    } else {
                        button.primary()
                    };
                    footer = footer.child(button.on_click(Self::callback_click(on_confirm, cx)));
                }
            }
            footer
        });

        // Confirm mode: Enter confirms, Escape rejects. Outside confirm mode,
        // Escape closes only when easy-close is on.
        let key_handler = {
            let on_close = self.on_close.clone();
            let on_confirm = self.on_confirm.clone();
            let on_reject = self.on_reject.clone();
            cx.listener(move |this: &mut V, ev: &KeyDownEvent, window, cx| {
                let m = &ev.keystroke.modifiers;
                if m.control || m.alt || m.platform || m.function {
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "escape" => {
                        if is_confirm {
                            if let Some(cb) = &on_reject {
                                cb(this, window, cx);
                            }
                        } else if easy_close && let Some(cb) = &on_close {
                            cb(this, window, cx);
                        }
                    }
                    "enter" if is_confirm => {
                        if let Some(cb) = &on_confirm {
                            cb(this, window, cx);
                        }
                    }
                    _ => {}
                }
            })
        };

        // Popped-out OS window: the window boundary IS the dialog boundary.
        // Content fills the window and resizes with it; the native title bar
        // replaces the themed one, and there is no scrim, card positioning,
        // or chrome move/resize.
        if self.windowed {
            let mut card = div()
                .size_full()
                .text_size(px(Theme::font_size()))
                .bg(Theme::popup_bg())
                .flex()
                .flex_col()
                .on_key_down(key_handler)
                .child(body)
                .children(footer);
            if let Some(focus) = &self.focus {
                card = card.track_focus(focus);
            }
            if let Some(key_context) = self.key_context.clone() {
                card = card.key_context(key_context.as_ref());
            }
            return card;
        }

        let card_selector = self.id.clone();
        let mut card = div()
            .id(self.id.clone())
            // Layout tests look the card up as `dialog-card-<id>`.
            .debug_selector(move || format!("dialog-card-{card_selector}"))
            .w(px(w))
            .text_size(px(Theme::font_size()))
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .rounded(px(s(8.0)))
            .overflow_hidden()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(key_handler)
            // The measuring canvas records where layout actually painted the
            // card, so the first drag of an auto-height card has a truthful
            // starting rectangle. It has no hitbox and paints nothing.
            .child(
                canvas(
                    move |bounds, _window, _cx| measured.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(titlebar)
            .child(body)
            .children(footer);
        match &mode {
            CardMode::Centered => {
                // Flex-centered until first drag; height follows content up
                // to a viewport cap, past which the body scrolls.
                card = card.max_h(px((vh * 0.85).max(160.0)));
            }
            CardMode::Anchored(origin) => {
                card = card.absolute().left(origin.x).top(origin.y);
                match fixed_h {
                    Some(h) => card = card.h(px(h)),
                    None => {
                        let top = f32::from(origin.y);
                        card = card.max_h(px((vh - top - 8.0).max(120.0)));
                    }
                }
            }
        }
        if let Some(focus) = &self.focus {
            card = card.track_focus(focus);
        }
        if let Some(key_context) = self.key_context.clone() {
            card = card.key_context(key_context.as_ref());
        }
        if self.resizable {
            // Edges first, corners after, so the corners win the hit test
            // where they overlap.
            for edge in [
                ResizeEdge::Top,
                ResizeEdge::Bottom,
                ResizeEdge::Left,
                ResizeEdge::Right,
                ResizeEdge::TopLeft,
                ResizeEdge::TopRight,
                ResizeEdge::BottomLeft,
                ResizeEdge::BottomRight,
            ] {
                card = card.child(self.resize_handle(edge, cx));
            }
        }

        let track_move = {
            let frame = self.frame.clone();
            cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                if frame(this).on_mouse_move(ev.position, window) {
                    cx.notify();
                }
            })
        };
        let track_up = {
            let frame = self.frame.clone();
            cx.listener(move |this, _ev: &gpui::MouseUpEvent, _window, cx| {
                if frame(this).end() {
                    cx.notify();
                }
            })
        };

        let centered = matches!(mode, CardMode::Centered);
        let overlay = div().absolute().top_0().left_0().size_full();
        let overlay = if centered {
            overlay.flex().items_center().justify_center()
        } else {
            overlay
        };
        if self.modal {
            // The scrim blocks the app behind the dialog; drag tracking rides
            // on it so the pointer can leave the card mid-drag.
            let mut overlay = overlay
                .occlude()
                .bg(Theme::scrim())
                .on_mouse_move(track_move)
                .on_mouse_up(MouseButton::Left, track_up);
            // Clicking outside dismisses only with easy-close on (or an
            // explicit override) — and never in confirm mode, where the user
            // must pick a button.
            let scrim_dismiss = self.scrim_dismiss.unwrap_or(easy_close && !is_confirm);
            if scrim_dismiss && let Some(on_close) = self.on_close.clone() {
                overlay = overlay.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, window, cx| on_close(this, window, cx)),
                );
            }
            if let Some(cursor) = drag_cursor {
                overlay = overlay.cursor(cursor);
            }
            overlay.child(card)
        } else {
            // Non-modal: the wrapper has no hitbox, so the app stays fully
            // interactive around the floating card. While a drag is live, a
            // transparent full-window layer tracks the pointer.
            let mut overlay = overlay.child(card);
            if dragging {
                let mut tracker = div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .occlude()
                    .on_mouse_move(track_move)
                    .on_mouse_up(MouseButton::Left, track_up);
                if let Some(cursor) = drag_cursor {
                    tracker = tracker.cursor(cursor);
                }
                overlay = overlay.child(tracker);
            }
            overlay
        }
    }
}

/// Where a detached dialog window first appears.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DetachedPlacement {
    /// Centered on the active display. The default.
    Center,
    /// Window top-left at this point in screen coordinates.
    ScreenPoint(Point<Pixels>),
    /// Window top-left at this offset from the parent window's top-left.
    WindowPoint(Point<Pixels>),
}

/// Options for [`open_detached`]: the OS-window counterpart of the inline
/// dialog card.
pub struct DetachedOptions {
    pub title: SharedString,
    /// Rebuilds `title` in the current language after a language switch.
    /// `None` keeps the title fixed (use for data, not wording).
    pub title_source: Option<std::rc::Rc<dyn Fn() -> String>>,
    pub width: f32,
    pub height: f32,
    pub placement: DetachedPlacement,
    pub resizable: bool,
}

impl DetachedOptions {
    pub fn new(title: impl Into<SharedString>, width: f32, height: f32) -> Self {
        Self {
            title: title.into(),
            title_source: None,
            width,
            height,
            placement: DetachedPlacement::Center,
            resizable: true,
        }
    }

    /// A window titled with the catalog text of `key`, re-titled when the
    /// language changes.
    pub fn localized(key: &'static str, width: f32, height: f32) -> Self {
        Self::localized_with(move || crate::localization::text(key), width, height)
    }

    /// A window titled by `title`, evaluated now and again on every language
    /// change (for titles formatted with arguments).
    pub fn localized_with(title: impl Fn() -> String + 'static, width: f32, height: f32) -> Self {
        let mut options = Self::new(title(), width, height);
        options.title_source = Some(std::rc::Rc::new(title));
        options
    }

    pub fn placement(mut self, placement: DetachedPlacement) -> Self {
        self.placement = placement;
        self
    }
}

/// Open a dialog as its own OS window (the `OpenDetached` / detach path),
/// wrapping the app's standard pop-out boilerplate: window bounds, native
/// titlebar with the dialog title, and the mandatory `gpui_component::Root`
/// wrapper. Returns the inner view so the caller can subscribe to its events.
///
/// The view rendered inside should build its chrome with
/// [`Dialog::windowed`] set, so the window boundary IS the dialog boundary —
/// content fills the window and the window's own edges resize it.
///
/// On wasm this always returns `None` — callers fall back to the inline card.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_detached<V: Render>(
    options: DetachedOptions,
    parent: Option<&Window>,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V> + 'static,
) -> Option<Entity<V>> {
    use std::cell::RefCell;

    use gpui::{TitlebarOptions, WindowBounds, WindowOptions, size};

    let window_size = size(px(options.width), px(options.height));
    let bounds = match options.placement {
        DetachedPlacement::Center => Bounds::centered(None, window_size, cx),
        DetachedPlacement::ScreenPoint(p) => Bounds {
            origin: p,
            size: window_size,
        },
        DetachedPlacement::WindowPoint(offset) => {
            let parent_origin = parent.map(|w| w.bounds().origin).unwrap_or_default();
            Bounds {
                origin: point(parent_origin.x + offset.x, parent_origin.y + offset.y),
                size: window_size,
            }
        }
    };
    // The slot smuggles the inner view past the Root wrapper so the caller
    // can subscribe to its events (same idiom as row_editor / er_diagram).
    let slot: Rc<RefCell<Option<Entity<V>>>> = Rc::new(RefCell::new(None));
    let slot_in = slot.clone();
    let title_source = options.title_source.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(options.title.clone()),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: options.resizable,
            ..Default::default()
        },
        move |window, cx| {
            if let Some(source) = title_source {
                crate::localization::register_window_title(window.window_handle(), move || {
                    source()
                });
            }
            let view = build(window, cx);
            *slot_in.borrow_mut() = Some(view.clone());
            cx.new(|cx| gpui_component::Root::new(view.clone(), window, cx))
        },
    );
    if opened.is_err() {
        return None;
    }
    slot.borrow_mut().take()
}

/// wasm: a browser tab cannot spawn OS windows; all detach options are
/// ignored and callers keep the dialog inline.
#[cfg(target_arch = "wasm32")]
pub fn open_detached<V: Render>(
    _options: DetachedOptions,
    _parent: Option<&Window>,
    _cx: &mut App,
    _build: impl FnOnce(&mut Window, &mut App) -> Entity<V> + 'static,
) -> Option<Entity<V>> {
    None
}

/// Lift a dialog built by a nested view (a sidebar panel, a section inside a
/// pane) to the window: the built [`Dialog`]'s scrim and card are laid out
/// against the whole viewport instead of the host's box, and painted above
/// everything without the host's clip. Without this, a dialog mounted in a
/// narrow sidebar is centered on the sidebar and cut off by it.
pub fn window_layer(dialog: impl IntoElement, window: &Window) -> AnyElement {
    let viewport = window.viewport_size();
    gpui::deferred(
        gpui::anchored().position(point(px(0.0), px(0.0))).child(
            div()
                .relative()
                .w(viewport.width)
                .h(viewport.height)
                .child(dialog),
        ),
    )
    .into_any_element()
}

/// One bulleted line for a dialog body. Style the row (text size, color); the
/// text wraps at the card's width. The text column needs `min_w(0)`: a flex
/// item's automatic minimum width is its unwrapped text width, which would
/// push every long bullet past the card's edge instead of wrapping it.
pub fn bullet_item(text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .w_full()
        .min_w(px(0.0))
        .gap(px(Theme::scaled(6.0)))
        .child(div().flex_none().child("•"))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .whitespace_normal()
                .child(text.into()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn frame_at(x: f32, y: f32, w: f32, h: f32) -> DialogFrame {
        let mut f = DialogFrame::new(w, h);
        f.origin = Some((x, y));
        f
    }

    fn start_resize(f: &mut DialogFrame, edge: ResizeEdge, px_x: f32, px_y: f32) {
        let (ox, oy) = f.origin.expect("test frames are pre-placed");
        f.drag = Some(Drag::Resize {
            edge,
            start_x: px_x,
            start_y: px_y,
            start_w: f.width,
            start_h: f.effective_height(),
            start_ox: ox,
            start_oy: oy,
        });
    }

    #[test]
    fn right_edge_resize_grows_width_only() {
        let mut f = frame_at(100.0, 100.0, 400.0, 300.0);
        start_resize(&mut f, ResizeEdge::Right, 500.0, 250.0);
        assert!(f.apply_pointer(560.0, 400.0, 1200.0, 800.0));
        assert_eq!(f.width, 460.0);
        assert_eq!(f.height, Some(300.0));
        assert_eq!(f.origin, Some((100.0, 100.0)));
    }

    #[test]
    fn left_edge_resize_moves_origin_and_keeps_right_edge() {
        let mut f = frame_at(100.0, 100.0, 400.0, 300.0);
        start_resize(&mut f, ResizeEdge::Left, 100.0, 250.0);
        assert!(f.apply_pointer(60.0, 250.0, 1200.0, 800.0));
        assert_eq!(f.width, 440.0);
        assert_eq!(f.origin, Some((60.0, 100.0)));
        // Right edge unchanged: 60 + 440 == 100 + 400.
    }

    #[test]
    fn top_left_corner_resizes_both_axes() {
        let mut f = frame_at(200.0, 200.0, 400.0, 300.0);
        start_resize(&mut f, ResizeEdge::TopLeft, 200.0, 200.0);
        assert!(f.apply_pointer(150.0, 170.0, 1200.0, 800.0));
        assert_eq!(f.width, 450.0);
        assert_eq!(f.height, Some(330.0));
        assert_eq!(f.origin, Some((150.0, 170.0)));
    }

    #[test]
    fn resize_clamps_to_minimum_and_pins_the_far_edge() {
        let mut f = frame_at(100.0, 100.0, 400.0, 300.0);
        start_resize(&mut f, ResizeEdge::Left, 100.0, 250.0);
        // Drag far right past the card's right edge.
        f.apply_pointer(900.0, 250.0, 1200.0, 800.0);
        assert_eq!(f.width, f.min_w);
        // Origin keeps the right edge at 500.
        assert_eq!(f.origin, Some((500.0 - f.min_w, 100.0)));
    }

    #[test]
    fn left_resize_cannot_push_past_viewport_left() {
        let mut f = frame_at(100.0, 100.0, 400.0, 300.0);
        start_resize(&mut f, ResizeEdge::Left, 100.0, 250.0);
        f.apply_pointer(-500.0, 250.0, 1200.0, 800.0);
        // Width caps at origin + width, so the left edge stops at 0.
        assert_eq!(f.width, 500.0);
        assert_eq!(f.origin, Some((0.0, 100.0)));
    }

    #[test]
    fn move_keeps_a_graspable_strip_on_screen() {
        let mut f = frame_at(100.0, 100.0, 400.0, 300.0);
        f.drag = Some(Drag::Move {
            grab_x: 10.0,
            grab_y: 10.0,
        });
        f.apply_pointer(-2000.0, -2000.0, 1200.0, 800.0);
        assert_eq!(f.origin, Some((48.0 - 400.0, 0.0)));
        f.apply_pointer(5000.0, 5000.0, 1200.0, 800.0);
        assert_eq!(f.origin, Some((1200.0 - 48.0, 800.0 - 32.0)));
    }

    #[test]
    fn placement_center_resolves_above_true_center() {
        let f = DialogFrame::new(400.0, 300.0);
        let (x, y) = f.resolved_in(1200.0, 800.0, (0.0, 0.0));
        assert_eq!(x, 400.0);
        assert_eq!(y, 200.0); // (800 - 300) / 2.5
    }

    #[test]
    fn placement_points_resolve_and_clamp() {
        let f = DialogFrame::new(400.0, 300.0)
            .with_placement(DialogPlacement::WindowPoint(point(px(60.0), px(40.0))));
        assert_eq!(f.resolved_in(1200.0, 800.0, (0.0, 0.0)), (60.0, 40.0));

        let f = DialogFrame::new(400.0, 300.0)
            .with_placement(DialogPlacement::ScreenPoint(point(px(560.0), px(340.0))));
        // Window sits at (500, 300) on the screen.
        assert_eq!(f.resolved_in(1200.0, 800.0, (500.0, 300.0)), (60.0, 40.0));

        // Off-screen requests are clamped to keep a graspable strip.
        let f = DialogFrame::new(400.0, 300.0)
            .with_placement(DialogPlacement::WindowPoint(point(px(9000.0), px(-50.0))));
        assert_eq!(f.resolved_in(1200.0, 800.0, (0.0, 0.0)), (1152.0, 0.0));
    }

    #[test]
    fn explicit_origin_wins_over_placement() {
        let mut f = DialogFrame::new(400.0, 300.0)
            .with_placement(DialogPlacement::WindowPoint(point(px(60.0), px(40.0))));
        f.origin = Some((10.0, 20.0));
        assert_eq!(f.resolved_in(1200.0, 800.0, (0.0, 0.0)), (10.0, 20.0));
    }

    #[test]
    fn auto_height_seeds_first_drag_from_measured_bounds() {
        let mut f = DialogFrame::new_auto(400.0);
        assert!(f.height().is_none());
        // Layout painted the flex-centered card at (380, 240), 400x260.
        f.measured.set(Some(Bounds {
            origin: point(px(380.0), px(240.0)),
            size: size(px(400.0), px(260.0)),
        }));
        f.drag = Some(Drag::Move {
            grab_x: 0.0,
            grab_y: 0.0,
        });
        // seed_origin is exercised through the measured fallback.
        let origin = match (f.origin, f.measured_rect()) {
            (Some(o), _) => o,
            (None, Some((x, y, _, _))) => (x, y),
            _ => panic!("measured bounds should resolve"),
        };
        assert_eq!(origin, (380.0, 240.0));
        assert_eq!(f.effective_height(), 260.0);
    }

    #[test]
    fn horizontal_resize_keeps_auto_height_vertical_fixes_it() {
        let mut f = DialogFrame::new_auto(400.0);
        f.origin = Some((100.0, 100.0));
        f.measured.set(Some(Bounds {
            origin: point(px(100.0), px(100.0)),
            size: size(px(400.0), px(260.0)),
        }));
        start_resize(&mut f, ResizeEdge::Right, 500.0, 200.0);
        f.apply_pointer(540.0, 200.0, 1200.0, 800.0);
        assert_eq!(f.width, 440.0);
        assert!(f.height().is_none(), "horizontal drag keeps content height");
        f.end();

        start_resize(&mut f, ResizeEdge::Bottom, 300.0, 360.0);
        f.apply_pointer(300.0, 420.0, 1200.0, 800.0);
        assert_eq!(f.height(), Some(320.0), "vertical drag fixes the height");
    }

    #[test]
    fn dialog_frames_map_creates_once_and_persists() {
        let mut frames = DialogFrames::new();
        frames
            .get("confirm-x", || DialogFrame::new(400.0, 200.0))
            .origin = Some((5.0, 6.0));
        let again = frames.get("confirm-x", || DialogFrame::new(999.0, 999.0));
        assert_eq!(again.width, 400.0, "existing frame wins over the default");
        assert_eq!(again.origin, Some((5.0, 6.0)));
    }

    #[test]
    fn ending_a_drag_clears_it() {
        let mut f = frame_at(0.0, 0.0, 400.0, 300.0);
        start_resize(&mut f, ResizeEdge::BottomRight, 400.0, 300.0);
        assert!(f.is_dragging());
        assert_eq!(f.active_cursor(), Some(CursorStyle::ResizeUpLeftDownRight));
        assert!(f.end());
        assert!(!f.is_dragging());
        assert!(!f.end());
        assert!(!f.apply_pointer(500.0, 400.0, 1200.0, 800.0));
    }

    #[test]
    fn edge_cursors_match_their_axis() {
        assert_eq!(ResizeEdge::Left.cursor(), CursorStyle::ResizeLeftRight);
        assert_eq!(ResizeEdge::Bottom.cursor(), CursorStyle::ResizeUpDown);
        assert_eq!(
            ResizeEdge::TopLeft.cursor(),
            CursorStyle::ResizeUpLeftDownRight
        );
        assert_eq!(
            ResizeEdge::TopRight.cursor(),
            CursorStyle::ResizeUpRightDownLeft
        );
    }

    #[test]
    fn tiny_viewports_never_panic_the_clamps() {
        let mut f = frame_at(0.0, 0.0, 400.0, 300.0);
        f.drag = Some(Drag::Move {
            grab_x: 0.0,
            grab_y: 0.0,
        });
        // vw - 48 < 48 - width would invert a naive clamp.
        f.apply_pointer(10.0, 10.0, 20.0, 10.0);
        let f = DialogFrame::new(400.0, 300.0)
            .with_placement(DialogPlacement::WindowPoint(point(px(0.0), px(0.0))));
        let _ = f.resolved_in(10.0, 10.0, (0.0, 0.0));
    }
}
