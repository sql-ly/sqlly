//! A form field's backing input state, single- or multi-line.
//!
//! gpui-component split its former unified `InputState` into per-mode types
//! (`InputState` / `TextareaState` / `EditorState`), so a form that keeps its
//! fields in one index-addressed array can no longer hold them in a
//! homogeneous `[Entity<InputState>; N]` when one field is multi-line. This
//! enum restores that shape for the two modes forms actually mix.

use gpui::{AnyElement, App, Context, Entity, IntoElement, Subscription, Window};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};

#[derive(Clone)]
pub enum FieldInput {
    Single(Entity<InputState>),
    Multi(Entity<TextareaState>),
}

impl FieldInput {
    pub fn value(&self, cx: &App) -> String {
        match self {
            Self::Single(input) => input.read(cx).value().to_string(),
            Self::Multi(input) => input.read(cx).value().to_string(),
        }
    }

    pub fn set_value(&self, value: String, window: &mut Window, cx: &mut App) {
        match self {
            Self::Single(input) => input.update(cx, |state, cx| state.set_value(value, window, cx)),
            Self::Multi(input) => input.update(cx, |state, cx| state.set_value(value, window, cx)),
        }
    }

    /// The styled element rendering this field: `Input` or `Textarea` to
    /// match the mode.
    pub fn element(&self) -> AnyElement {
        match self {
            Self::Single(input) => Input::new(input).into_any_element(),
            Self::Multi(input) => Textarea::new(input).into_any_element(),
        }
    }

    /// Subscribe `handler` to this field's [`InputEvent`]s, whichever mode it
    /// is. Both modes emit the same event type.
    pub fn subscribe<V: 'static>(
        &self,
        window: &mut Window,
        cx: &mut Context<V>,
        handler: impl Fn(&mut V, &InputEvent, &mut Window, &mut Context<V>) + 'static,
    ) -> Subscription {
        match self {
            Self::Single(input) => cx.subscribe_in(
                input,
                window,
                move |view, _, event: &InputEvent, window, cx| handler(view, event, window, cx),
            ),
            Self::Multi(input) => cx.subscribe_in(
                input,
                window,
                move |view, _, event: &InputEvent, window, cx| handler(view, event, window, cx),
            ),
        }
    }
}
