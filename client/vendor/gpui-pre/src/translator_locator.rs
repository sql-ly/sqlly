//! SQLLY patch: hooks the translation manager preview uses to find the
//! visible text that belongs to a translation key. The host decides which
//! strings are tracked; gpui only reports text elements as they prepaint and
//! gives the host one paint slot at the end of each frame. Text is never
//! rewritten, so nothing here can leak into shaping, input buffers or copies.

use crate::{Bounds, Hitbox, Pixels, Point, Window};
use std::cell::Cell;

/// Callbacks installed by the application. All are plain `fn`s so the hooks
/// cost one thread-local read when the host is idle.
#[derive(Clone, Copy)]
pub struct TranslatorLocatorHooks {
    /// Runs before a window frame draws. Returning `true` makes the frame
    /// repaint cached views too, so every tracked text reports itself.
    pub frame_started: fn() -> bool,
    /// Runs for every text element during prepaint with its unshaped text and
    /// bounds. The host may insert a hitbox here.
    pub text_prepainted: fn(&str, Bounds<Pixels>, &mut Window),
    /// Runs once at the end of a frame's paint phase, above all content, so
    /// the host can paint outlines and register input listeners.
    pub frame_painted: fn(&mut Window),
}

thread_local! {
    static HOOKS: Cell<Option<TranslatorLocatorHooks>> = const { Cell::new(None) };
}

/// Installs the application's translator locator hooks for this WASM thread.
pub fn set_translator_locator_hooks(hooks: TranslatorLocatorHooks) {
    HOOKS.with(|slot| slot.set(Some(hooks)));
}

/// Whether `hitbox` is the topmost mouse target at `point` in the frame being
/// painted, so text under a dialog's backdrop is not treated as visible. Only
/// meaningful from [`TranslatorLocatorHooks::frame_painted`].
pub fn translator_hitbox_visible_at(
    window: &Window,
    hitbox: &Hitbox,
    point: Point<Pixels>,
) -> bool {
    let hit_test = window.next_frame.hit_test(point);
    hit_test
        .ids
        .iter()
        .take(hit_test.hover_hitbox_count)
        .any(|id| *id == hitbox.id)
}

pub(crate) fn frame_started() -> bool {
    HOOKS.with(|slot| slot.get().is_some_and(|hooks| (hooks.frame_started)()))
}

pub(crate) fn text_prepainted(text: &str, bounds: Bounds<Pixels>, window: &mut Window) {
    if let Some(hooks) = HOOKS.with(Cell::get) {
        (hooks.text_prepainted)(text, bounds, window);
    }
}

pub(crate) fn frame_painted(window: &mut Window) {
    if let Some(hooks) = HOOKS.with(Cell::get) {
        (hooks.frame_painted)(window);
    }
}
