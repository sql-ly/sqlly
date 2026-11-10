//! Shared frames for form controls: the label column, field wrapper, and
//! checkbox row the Settings window uses, lifted out so other dialogs render
//! their inputs at the same size, color, and alignment.
//!
//! The gpui-component widgets (`Input`, `Select`, `ColorPicker`, `Checkbox`)
//! theme their own chrome from the synced global theme. What they do NOT
//! carry is the text color of an `Input`'s value (inherited, and gpui's
//! default is black) or a consistent label column — that is what lives here.
//! Render the widgets at their default size (not `.small()`), as Settings
//! does, so every form in the app has the same control height.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};

use crate::theme::Theme;

/// Minimum width of the label column in a [`labeled_row`].
pub const LABEL_MIN_W: f32 = 180.;
/// Gap between the label column and its control.
pub const LABEL_GAP: f32 = 12.;
/// Minimum width of a single field (text input, dropdown, value box).
pub const FIELD_MIN_W: f32 = 220.;

/// Left inset that lines content (an error, a lone checkbox) up with the
/// control column of a [`labeled_row`].
pub fn control_column_inset() -> f32 {
    Theme::scaled(LABEL_MIN_W + LABEL_GAP)
}

/// A label on the left, its control on the right — the Settings row.
pub fn labeled_row(label: impl IntoElement, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(LABEL_GAP)))
        .py(px(Theme::scaled(4.)))
        .child(
            div()
                .text_color(Theme::text())
                .text_size(px(Theme::font_size()))
                .min_w(px(Theme::scaled(LABEL_MIN_W)))
                .child(label),
        )
        .child(control)
        .into_any_element()
}

/// Wraps a text `Input` (or a `Select`) in the field frame: the minimum
/// width, and a pinned text color. `Input` paints its value with the
/// INHERITED text color (only the placeholder is themed internally), and
/// gpui's default text color is black, so a container that fails to provide
/// a themed color leaves dark-mode fields black-on-dark.
pub fn field(control: impl IntoElement) -> gpui::Div {
    div()
        .min_w(px(Theme::scaled(FIELD_MIN_W)))
        .text_color(Theme::text())
        .child(control)
}

/// Wraps a `Checkbox` so it sizes to its label. Left bare in a `flex_col`, the
/// container stretches edge-to-edge and the focus ring draws a box across the
/// whole form instead of around the control.
pub fn checkbox_row(checkbox: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .py(px(Theme::scaled(2.)))
        .text_color(Theme::text())
        .child(checkbox)
}
