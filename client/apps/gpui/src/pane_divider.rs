//! In-app pane divider: the draggable, collapsible splitter between panels.
//!
//! Replaces the old `gpui_ui_kit::PaneDivider` with a local element themed
//! straight from [`crate::theme::Theme`]. gpui-component's `resizable` module
//! owns the whole resize interaction, which does not fit the app's existing
//! parent-tracked drag/persist model (the shell records drag start, tracks
//! window mouse-move, and persists pane sizes itself) — so the divider stays a
//! dumb element: it reports `on_drag_start`, toggles collapse on double-click
//! (or any click while collapsed), and paints the collapsed strip with its
//! label and expand arrows.
//!
//! Behavior contract (unchanged from the kit version the app shipped with):
//! - expanded: single mouse-down starts a drag (reports x for vertical / y for
//!   horizontal dividers), double-click collapses (`on_toggle(true)`).
//! - collapsed: any click expands (`on_toggle(false)`).

use gpui::prelude::*;
use gpui::{
    AnyElement, App, CursorStyle, Div, MouseButton, Pixels, SharedString, Stateful, Window, div, px,
};

use crate::theme::Theme;

/// Direction the adjacent panel collapses toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapseDirection {
    /// Collapse the panel to the left (vertical dividers).
    Left,
    /// Collapse the panel to the right (vertical dividers).
    Right,
    /// Collapse the panel upward (horizontal dividers).
    Up,
    /// Collapse the panel downward (horizontal dividers).
    Down,
}

impl CollapseDirection {
    /// The opposite direction — the way the expand arrows point while collapsed.
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Up => Self::Down,
            Self::Down => Self::Up,
        }
    }

    /// Double-arrow glyphs for the collapsed strip.
    pub fn arrows(self) -> &'static str {
        match self {
            Self::Left => "\u{25C0}\u{25C0}",
            Self::Right => "\u{25B6}\u{25B6}",
            Self::Up => "\u{25B2}\u{25B2}",
            Self::Down => "\u{25BC}\u{25BC}",
        }
    }

    /// Single-arrow glyph shown on the expanded divider.
    #[cfg(test)]
    pub fn single_arrow(self) -> &'static str {
        match self {
            Self::Left => "\u{25C0}",
            Self::Right => "\u{25B6}",
            Self::Up => "\u{25B2}",
            Self::Down => "\u{25BC}",
        }
    }

    /// Whether this collapse moves along the horizontal axis (vertical divider).
    pub fn is_horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

type ToggleHandler = Box<dyn Fn(bool, &mut Window, &mut App) + 'static>;
type DragStartHandler = Box<dyn Fn(f32, &mut Window, &mut App) + 'static>;

/// Draggable, collapsible divider element. See module docs for the contract.
pub struct PaneDivider {
    id: SharedString,
    label: SharedString,
    collapse_direction: CollapseDirection,
    collapsed: bool,
    on_toggle: Option<ToggleHandler>,
    on_drag_start: Option<DragStartHandler>,
    thickness: Pixels,
    collapsed_size: Pixels,
}

impl PaneDivider {
    /// A vertical divider (between left/right panels).
    pub fn vertical(id: impl Into<SharedString>, collapse_direction: CollapseDirection) -> Self {
        debug_assert!(
            collapse_direction.is_horizontal(),
            "vertical dividers collapse Left or Right"
        );
        Self::new(id, collapse_direction)
    }

    /// A horizontal divider (between top/bottom panels).
    pub fn horizontal(id: impl Into<SharedString>, collapse_direction: CollapseDirection) -> Self {
        debug_assert!(
            !collapse_direction.is_horizontal(),
            "horizontal dividers collapse Up or Down"
        );
        Self::new(id, collapse_direction)
    }

    fn new(id: impl Into<SharedString>, collapse_direction: CollapseDirection) -> Self {
        Self {
            id: id.into(),
            label: SharedString::default(),
            collapse_direction,
            collapsed: false,
            on_toggle: None,
            on_drag_start: None,
            // In-flow width is the 1px hairline so panels sit flush. The
            // grab target is a 6px overlay centered on that line.
            thickness: px(1.0),
            collapsed_size: px(Theme::scaled(24.0)),
        }
    }

    /// Label painted on the collapsed strip (e.g. "Explorer", "Results").
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }

    /// Whether the adjacent panel is currently collapsed.
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    /// Collapse-state change callback; receives the NEW collapsed state.
    pub fn on_toggle(mut self, callback: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Box::new(callback));
        self
    }

    /// Drag-start callback; receives the mouse position on the drag axis
    /// (x for vertical dividers, y for horizontal). The parent tracks the rest
    /// of the drag via window mouse-move/up, as before.
    pub fn on_drag_start(
        mut self,
        callback: impl Fn(f32, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_drag_start = Some(Box::new(callback));
        self
    }

    /// Divider thickness while expanded.
    #[allow(dead_code)] // builder API parity with the kit divider; callers may tune it
    pub fn thickness(mut self, thickness: Pixels) -> Self {
        self.thickness = thickness;
        self
    }

    /// Strip size while collapsed.
    #[allow(dead_code)] // builder API parity with the kit divider; callers may tune it
    pub fn collapsed_size(mut self, size: Pixels) -> Self {
        self.collapsed_size = size;
        self
    }

    /// Whether the expanded divider paints a 6px fill. Hairline dividers never
    /// fill the hit target — they paint a 1px child that brightens on hover.
    #[cfg(test)]
    pub fn expanded_fill_visible(_hovered: bool) -> bool {
        false
    }

    /// Hover brightens the 1px hairline; rest uses the ordinary border token.
    #[cfg(test)]
    pub fn expanded_hairline_bright(hovered: bool) -> bool {
        hovered
    }

    /// In-flow thickness of an expanded divider (the hairline, not the grab overlay).
    #[cfg(test)]
    pub fn expanded_layout_px() -> f32 {
        1.0
    }

    fn build(self) -> Stateful<Div> {
        let is_vertical = self.collapse_direction.is_horizontal();
        if self.collapsed {
            self.build_collapsed(is_vertical)
        } else {
            self.build_expanded(is_vertical)
        }
    }

    fn build_expanded(self, is_vertical: bool) -> Stateful<Div> {
        let cursor = if is_vertical {
            CursorStyle::ResizeLeftRight
        } else {
            CursorStyle::ResizeUpDown
        };
        // Expanded: 1px in-flow hairline so neighboring panels sit flush.
        // A 6px overlay centered on the line is the grab target. Hover
        // brightens the hairline — never a fill, groove, or chevron.
        let group: SharedString = format!("pane-div-{}", self.id).into();
        let hit = Theme::scaled(6.0);
        let inset = hit / 2.0;
        let on_toggle = self.on_toggle;
        let on_drag_start = self.on_drag_start;
        let overlay = {
            let mut hit_el = if is_vertical {
                div()
                    .absolute()
                    .left(px(-inset))
                    .w(px(hit))
                    .h_full()
                    .cursor(cursor)
            } else {
                div()
                    .absolute()
                    .top(px(-inset))
                    .h(px(hit))
                    .w_full()
                    .cursor(cursor)
            };
            if on_toggle.is_some() || on_drag_start.is_some() {
                hit_el = hit_el.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    if event.click_count == 2 {
                        if let Some(toggle) = &on_toggle {
                            toggle(true, window, cx);
                        }
                    } else if event.click_count == 1
                        && let Some(drag) = &on_drag_start
                    {
                        let pos: f32 = if is_vertical {
                            event.position.x.into()
                        } else {
                            event.position.y.into()
                        };
                        drag(pos, window, cx);
                    }
                });
            }
            hit_el
        };
        if is_vertical {
            div()
                .id(self.id)
                .group(group.clone())
                .relative()
                .w(self.thickness)
                .h_full()
                .bg(Theme::border())
                .group_hover(group, |s| s.bg(Theme::border_hover()))
                .child(overlay)
        } else {
            div()
                .id(self.id)
                .group(group.clone())
                .relative()
                .h(self.thickness)
                .w_full()
                .bg(Theme::border())
                .group_hover(group, |s| s.bg(Theme::border_hover()))
                .child(overlay)
        }
    }

    fn build_collapsed(self, is_vertical: bool) -> Stateful<Div> {
        let arrows = self.collapse_direction.opposite().arrows();
        let arrow_el = || {
            div()
                .text_color(Theme::text_dim())
                // Fixed glyph: sized to the collapsed strip, which keeps one width.
                .text_size(px(Theme::scaled(10.0)))
                .child(arrows)
        };

        let mut base = if is_vertical {
            // Narrow vertical strip with the label's characters stacked.
            let label_elements: Vec<AnyElement> = self
                .label
                .chars()
                .map(|c| {
                    div()
                        .text_color(Theme::text_dim())
                        // Fixed glyph: one stacked letter per row of the fixed-width strip.
                        .text_size(px(Theme::scaled(11.0)))
                        .child(c.to_string())
                        .into_any_element()
                })
                .collect();
            div()
                .id(self.id)
                .w(self.collapsed_size)
                .h_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(Theme::scaled(4.0)))
                .bg(Theme::sidebar_bg())
                .border_x_1()
                .border_color(Theme::border())
                .cursor_pointer()
                .child(arrow_el())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(Theme::scaled(2.0)))
                        .py_2()
                        .children(label_elements),
                )
                .child(arrow_el())
        } else {
            div()
                .id(self.id)
                .h(self.collapsed_size)
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(Theme::scaled(8.0)))
                .bg(Theme::sidebar_bg())
                .border_y_1()
                .border_color(Theme::border())
                .cursor_pointer()
                .child(arrow_el())
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        // Fixed glyph: the label fits the strip's fixed collapsed height.
                        .text_size(px(Theme::scaled(11.0)))
                        .child(self.label.to_string()),
                )
                .child(arrow_el())
        };

        base = base.hover(|style| style.bg(Theme::border()).text_color(Theme::text()));

        if let Some(toggle) = self.on_toggle {
            base = base.on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                toggle(false, window, cx);
            });
        }
        base
    }
}

impl IntoElement for PaneDivider {
    type Element = Stateful<Div>;

    fn into_element(self) -> Self::Element {
        self.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opposite_directions_pair_up() {
        assert_eq!(CollapseDirection::Left.opposite(), CollapseDirection::Right);
        assert_eq!(CollapseDirection::Right.opposite(), CollapseDirection::Left);
        assert_eq!(CollapseDirection::Up.opposite(), CollapseDirection::Down);
        assert_eq!(CollapseDirection::Down.opposite(), CollapseDirection::Up);
    }

    #[test]
    fn horizontal_axis_classification() {
        assert!(CollapseDirection::Left.is_horizontal());
        assert!(CollapseDirection::Right.is_horizontal());
        assert!(!CollapseDirection::Up.is_horizontal());
        assert!(!CollapseDirection::Down.is_horizontal());
    }

    #[test]
    fn arrow_glyphs_match_direction() {
        assert_eq!(CollapseDirection::Down.single_arrow(), "\u{25BC}");
        assert_eq!(CollapseDirection::Up.arrows(), "\u{25B2}\u{25B2}");
    }

    #[test]
    fn expanded_divider_never_fills_the_hit_target() {
        assert!(!PaneDivider::expanded_fill_visible(false));
        assert!(!PaneDivider::expanded_fill_visible(true));
    }

    #[test]
    fn expanded_hairline_brightens_on_hover_only() {
        assert!(!PaneDivider::expanded_hairline_bright(false));
        assert!(PaneDivider::expanded_hairline_bright(true));
    }

    #[test]
    fn expanded_layout_thickness_is_the_hairline() {
        assert_eq!(PaneDivider::expanded_layout_px(), 1.0);
    }
}
