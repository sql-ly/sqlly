//! Translation manager preview support: outlines the visible text of the
//! selected translation key and lets a translator pick text to open its key.
//!
//! `sqlly_i18n` records which keys produced each resolved wording while this
//! is active; the patched gpui text elements report their text as they
//! prepaint, and every recorded wording gets a hitbox so clicks and outlines
//! respect dialogs and other occluding layers. Wording shared by several keys
//! is reported with all of them and outlined dashed when one is selected.

use crate::theme::Theme;
use gpui::{
    App, BorderStyle, Bounds, CursorStyle, DispatchPhase, Hitbox, HitboxBehavior, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    TranslatorLocatorHooks, Window, outline, px, translator_hitbox_visible_at,
};
use sqlly_i18n::TranslatorKeys as Keys;
use std::cell::RefCell;

/// Host notifications: a picked region's keys (most recently resolved first)
/// and pick mode ending inside the preview (Escape).
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub picked: fn(&[Box<str>]),
    pub pick_mode_changed: fn(bool),
}

#[derive(Default)]
struct State {
    selected: Option<String>,
    picking: bool,
    /// The left button went down on a pick; its release must not reach the
    /// control underneath (menu items activate on mouse up).
    swallow_mouse_up: bool,
    hovered: Option<Bounds<Pixels>>,
    regions: Vec<Region>,
    callbacks: Option<Callbacks>,
}

struct Region {
    keys: Keys,
    /// Painted bounds, clipped to the element's content mask.
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn initialize(callbacks: Callbacks) {
    STATE.with(|state| state.borrow_mut().callbacks = Some(callbacks));
    gpui::set_translator_locator_hooks(TranslatorLocatorHooks {
        frame_started,
        text_prepainted,
        frame_painted,
    });
}

pub fn highlight(key: String, cx: &mut App) {
    let was_active = active();
    STATE.with(|state| state.borrow_mut().selected = Some(key));
    apply(was_active, cx);
}

pub fn clear(cx: &mut App) {
    let was_active = active();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.selected = None;
        state.picking = false;
        state.hovered = None;
    });
    apply(was_active, cx);
}

pub fn set_picking(enabled: bool, cx: &mut App) {
    let was_active = active();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.picking = enabled;
        state.hovered = None;
    });
    apply(was_active, cx);
}

/// Wordings are only recorded while tracking is on, so turning it on
/// re-resolves every localized string; any other change just repaints.
fn apply(was_active: bool, cx: &mut App) {
    let active = active();
    sqlly_i18n::set_translator_locator_enabled(active);
    if active && !was_active {
        crate::localization::refresh(cx);
    } else {
        cx.refresh_windows();
    }
}

fn active() -> bool {
    STATE.with(|state| {
        let state = state.borrow();
        state.selected.is_some() || state.picking
    })
}

fn frame_started() -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.regions.clear();
        state.selected.is_some() || state.picking
    })
}

fn text_prepainted(text: &str, bounds: Bounds<Pixels>, window: &mut Window) {
    if !active() {
        return;
    }
    let Some(keys) = sqlly_i18n::translator_keys_for(text) else {
        return;
    };
    let clipped = bounds.intersect(&window.content_mask().bounds);
    if clipped.is_empty() {
        return;
    }
    let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
    STATE.with(|state| {
        state.borrow_mut().regions.push(Region {
            keys,
            bounds: clipped,
            hitbox,
        })
    });
}

/// The topmost recorded region under `position` in the frame on screen.
fn region_at(position: Point<Pixels>, window: &Window) -> Option<(Bounds<Pixels>, Keys)> {
    STATE.with(|state| {
        state
            .borrow()
            .regions
            .iter()
            .rev()
            .find(|region| {
                region.bounds.contains(&position) && region.hitbox.is_hovered_at(position, window)
            })
            .map(|region| (region.bounds, region.keys.clone()))
    })
}

fn frame_painted(window: &mut Window) {
    let (picking, swallow_mouse_up) = STATE.with(|state| {
        let state = state.borrow();
        (state.picking, state.swallow_mouse_up)
    });
    if picking || swallow_mouse_up {
        register_pick_listeners(window);
    }
    if !active() {
        return;
    }
    let mouse = window.mouse_position();
    let outlines = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let hovered = if state.picking {
            state
                .regions
                .iter()
                .rev()
                .find(|region| {
                    region.bounds.contains(&mouse)
                        && translator_hitbox_visible_at(window, &region.hitbox, mouse)
                })
                .map(|region| region.bounds)
        } else {
            None
        };
        state.hovered = hovered;
        let mut outlines = Vec::new();
        for region in &state.regions {
            let selected = state
                .selected
                .as_deref()
                .is_some_and(|key| region.keys.iter().any(|candidate| &**candidate == key));
            if selected
                && translator_hitbox_visible_at(window, &region.hitbox, region.bounds.center())
            {
                let style = if region.keys.len() == 1 {
                    BorderStyle::Solid
                } else {
                    BorderStyle::Dashed
                };
                outlines.push((region.bounds, Theme::error(), style));
            }
        }
        if let Some(bounds) = hovered {
            outlines.push((bounds, Theme::accent(), BorderStyle::Solid));
        }
        outlines
    });
    for (bounds, color, style) in outlines {
        // A background-coloured band under the ring, one pixel wider on each
        // side, keeps it visible on a fill of its own colour: the accent hover
        // ring over the accent Run button was otherwise invisible.
        window.paint_quad(
            outline(bounds.dilate(px(1.0)), Theme::bg(), BorderStyle::Solid).border_widths(px(4.0)),
        );
        window.paint_quad(outline(bounds, color, style).border_widths(px(2.0)));
    }
    if picking {
        window.set_window_cursor_style(CursorStyle::Crosshair);
    }
}

fn register_pick_listeners(window: &mut Window) {
    window.on_mouse_event(|event: &MouseMoveEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture || !picking() {
            return;
        }
        let hovered = region_at(event.position, window).map(|(bounds, _)| bounds);
        if STATE.with(|state| state.borrow().hovered != hovered) {
            window.refresh();
        }
        cx.stop_propagation();
    });
    window.on_mouse_event(|event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture || !picking() {
            return;
        }
        cx.stop_propagation();
        if event.button != MouseButton::Left {
            return;
        }
        let Some((_, keys)) = region_at(event.position, window) else {
            return;
        };
        let callbacks = STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.selected = Some(keys[0].to_string());
            state.picking = false;
            state.hovered = None;
            state.swallow_mouse_up = true;
            state.callbacks
        });
        if let Some(callbacks) = callbacks {
            (callbacks.picked)(&keys);
        }
        cx.refresh_windows();
    });
    window.on_mouse_event(|_: &MouseUpEvent, phase, _, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        let swallow = STATE.with(|state| {
            let mut state = state.borrow_mut();
            std::mem::take(&mut state.swallow_mouse_up) || state.picking
        });
        if swallow {
            cx.stop_propagation();
        }
    });
    window.on_key_event(|event: &KeyDownEvent, phase, _, cx| {
        if phase != DispatchPhase::Capture || !picking() || event.keystroke.key != "escape" {
            return;
        }
        cx.stop_propagation();
        let callbacks = STATE.with(|state| state.borrow().callbacks);
        set_picking(false, cx);
        if let Some(callbacks) = callbacks {
            (callbacks.pick_mode_changed)(false);
        }
    });
}

fn picking() -> bool {
    STATE.with(|state| state.borrow().picking)
}
