//! The pill tab strip used in panel headers.
//!
//! The AI panel drew its section pills inline; the sidebar header drew a
//! different underlined tab bar. They sit side by side on the same chrome row,
//! so they now share one look from here.
//!
//! The active pill is filled with the hover-row tone rather than the panel
//! background: both panels sit on the sidebar rail tone, where the panel
//! background is invisible.

use gpui::{App, IntoElement, MouseButton, Styled, Window, div, prelude::*, px};

use crate::theme::Theme;

/// A row of pill tabs. `items` is `(value, label)` in display order; the tab
/// whose value equals `selected` is painted active, and clicking any tab calls
/// `on_select` with its value.
pub fn chrome_tabs<T: Copy + PartialEq + 'static, L: Into<String>>(
    id: &'static str,
    items: impl IntoIterator<Item = (T, L)>,
    selected: T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let s = Theme::scaled;
    let on_select = std::rc::Rc::new(on_select);
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(2.0)))
        .children(items.into_iter().map(|(value, label)| {
            let is_active = value == selected;
            let on_select = on_select.clone();
            div()
                .px(px(s(8.0)))
                .py(px(s(3.0)))
                .rounded(px(s(3.0)))
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(if is_active {
                    Theme::text()
                } else {
                    Theme::text_dim()
                })
                .bg(if is_active {
                    Theme::row_hover()
                } else {
                    gpui::transparent_black()
                })
                .when(!is_active, |d| d.hover(|s| s.text_color(Theme::text())))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(label.into())
                .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                    on_select(value, window, cx);
                })
        }))
}
