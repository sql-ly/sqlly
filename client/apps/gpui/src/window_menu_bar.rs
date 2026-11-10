//! In-window menu bar for platforms without a native application menu.
//!
//! GPUI only installs `cx.set_menus(...)` natively on macOS; on Windows and
//! Linux the menus are merely stored. This view renders the same structure
//! [`crate::menu_bar::build_menus`] produces as a horizontal bar at the top
//! of the window and dispatches the same actions through
//! `window.dispatch_action`, so both platforms share one command surface.
//!
//! Behavior follows native Windows menu bars: click a title to open its
//! dropdown, hover across titles while one is open to switch menus, click an
//! item to run it, click anywhere else to dismiss. Dropdowns are rendered
//! with `deferred(anchored(...))` + `.occlude()` + `on_mouse_down_out` so
//! they paint above all content, never clip, and dismiss rationally.

use crate::tooltip::AppTooltip;
use gpui::{
    AnyElement, ClickEvent, Context, Hsla, IntoElement, KeyDownEvent, Keystroke, MouseButton,
    MouseDownEvent, MouseMoveEvent, OwnedMenu, OwnedMenuItem, Pixels, Point, Render, Styled,
    Window, WindowControlArea, anchored, deferred, div, img, point, prelude::*, px,
};
use gpui_component::Icon;

use crate::icon_catalog::AppIcon;
use crate::theme::Theme;

/// Width of each window-control button (min / max / close).
const CONTROL_WIDTH: f32 = 46.0;

/// Logical size of the SQLLY pig in the title bar. Source art is 32×32 (@2x).
const TITLEBAR_MARK_PX: f32 = 16.0;

/// Optical nudge so the pig sits on the wordmark’s cap-height, not geometrically centered (the snout reads high).
const TITLEBAR_MARK_NUDGE_Y: f32 = 2.0;

/// How far the pointer must travel while held before a caption press counts
/// as a window drag rather than a click. A hand always jitters a pixel or two
/// between the press and release of a double-click, and on Wayland treating
/// that as a drag is fatal: `start_window_move` makes the compositor take a
/// pointer grab and send us a leave, so the release never arrives and the
/// click never completes.
///
/// Deliberately equal to gpui's own `DOUBLE_CLICK_DISTANCE`
/// (`platform/linux/platform.rs`, 5px): within that radius a second press can
/// still pair into a double-click, so moving would destroy it; beyond it gpui
/// will never report `click_count == 2` anyway, so there is nothing left to
/// protect and the drag should start at once. A larger value would only add a
/// dead zone where neither gesture responds.
const DRAG_THRESHOLD: f32 = 5.0;

/// Logical inset for native macOS traffic lights in the transparent titlebar.
pub fn macos_traffic_light_position() -> Point<Pixels> {
    point(px(12.0), px(11.0))
}

/// Left padding so the wordmark does not sit under macOS traffic lights.
pub fn macos_title_bar_leading_pad() -> f32 {
    78.0
}

/// Dim at rest, full ink when that panel is open.
pub fn layout_toggle_ink(active: bool) -> Hsla {
    if active {
        Theme::text()
    } else {
        Theme::text_dim()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutToggleState {
    pub sidebar: bool,
    pub ai_drawer: bool,
    pub log: bool,
    pub ai_in_flight: bool,
}

pub struct WindowMenuBar {
    menus: Vec<OwnedMenu>,
    /// Engine-gated actions that cannot run on the active connection, keyed
    /// by action name, mapped to the user-facing capability reason. Rows for
    /// these actions render dimmed and inert, with the reason where the
    /// shortcut hint would sit.
    disabled_actions: std::collections::HashMap<&'static str, String>,
    /// Index of the currently-open top-level menu, if any.
    open: Option<usize>,
    /// Focused action within the open dropdown (Windows/Linux keyboard nav).
    item_focus: Option<usize>,
    /// Typeahead buffer for jumping to a menu item by name.
    typeahead: String,
    /// Where the left button went down in the caption area, while it is still
    /// ambiguous whether this is a click or a drag. Cleared once the move
    /// starts or the button comes back up.
    caption_press: Option<Point<Pixels>>,
    layout: LayoutToggleState,
}

/// Flatten action names from a menu (skipping separators).
pub fn menu_action_names(items: &[OwnedMenuItem]) -> Vec<String> {
    let mut names = Vec::new();
    collect_action_names(items, &mut names);
    names
}

fn collect_action_names(items: &[OwnedMenuItem], names: &mut Vec<String>) {
    for item in items {
        match item {
            OwnedMenuItem::Action { name, .. } => names.push(name.to_string()),
            OwnedMenuItem::Submenu(submenu) => collect_action_names(&submenu.items, names),
            OwnedMenuItem::Separator | OwnedMenuItem::SystemMenu(_) => {}
        }
    }
}

/// Step through action items, wrapping. `None` starts at the end for backward
/// and at the last item so a first Down lands on 0.
pub fn step_menu_item_focus(current: Option<usize>, count: usize, delta: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let start = match current {
        Some(i) => i as isize,
        None => {
            if delta > 0 {
                -1
            } else {
                count as isize
            }
        }
    };
    Some((start + delta).rem_euclid(count as isize) as usize)
}

/// First item at or after `from` whose name starts with `prefix` (case-insensitive).
pub fn typeahead_menu_index(names: &[String], from: usize, prefix: &str) -> Option<usize> {
    if prefix.is_empty() || names.is_empty() {
        return None;
    }
    let p = prefix.to_lowercase();
    let n = names.len();
    for i in 0..n {
        let idx = (from + i) % n;
        if names[idx].to_lowercase().starts_with(&p) {
            return Some(idx);
        }
    }
    None
}

fn nth_menu_action(items: &[OwnedMenuItem], mut n: usize) -> Option<Box<dyn gpui::Action>> {
    fn walk(items: &[OwnedMenuItem], n: &mut usize) -> Option<Box<dyn gpui::Action>> {
        for item in items {
            match item {
                OwnedMenuItem::Action { action, .. } => {
                    if *n == 0 {
                        return Some(action.boxed_clone());
                    }
                    *n -= 1;
                }
                OwnedMenuItem::Submenu(submenu) => {
                    if let Some(found) = walk(&submenu.items, n) {
                        return Some(found);
                    }
                }
                OwnedMenuItem::Separator | OwnedMenuItem::SystemMenu(_) => {}
            }
        }
        None
    }
    walk(items, &mut n)
}

impl WindowMenuBar {
    pub fn new(menus: Vec<gpui::Menu>) -> Self {
        Self {
            menus: menus.into_iter().map(|m| m.owned()).collect(),
            disabled_actions: std::collections::HashMap::new(),
            open: None,
            item_focus: None,
            typeahead: String::new(),
            caption_press: None,
            layout: LayoutToggleState::default(),
        }
    }

    /// Returns whether the toggles changed, so the caller can notify only
    /// then: the bar is frame-cached and re-renders on its own notifies.
    pub fn set_layout_toggles(&mut self, layout: LayoutToggleState) -> bool {
        let changed = self.layout != layout;
        self.layout = layout;
        changed
    }

    /// The bar's root layout style, for rendering it from the frame cache
    /// (`Entity::cached` skips `render`, so the root style is supplied here).
    pub fn cached_root_style() -> gpui::StyleRefinement {
        let s = Theme::scaled;
        let item_h = Theme::line_height() + s(4.5);
        let bar_h = item_h + s(10.0);
        gpui::StyleRefinement::default()
            .w_full()
            .h(px(bar_h))
            .flex_none()
    }

    fn focused_action_name(&self) -> Option<String> {
        let i = self.open?;
        let names = menu_action_names(&self.menus.get(i)?.items);
        names.get(self.item_focus?).cloned()
    }

    /// Replace the rendered menu set. Called when the app rebuilds its menus —
    /// e.g. the Tools ▸ Connect/Disconnect item flipping on a connection change.
    pub fn set_menus(&mut self, menus: Vec<gpui::Menu>) {
        self.menus = menus.into_iter().map(|m| m.owned()).collect();
    }

    /// Replace the set of engine-gated actions to gray out, keyed by action
    /// name with their capability reasons. Set alongside `set_menus` when the
    /// active connection or its engine changes.
    pub fn set_disabled_actions(
        &mut self,
        disabled: std::collections::HashMap<&'static str, String>,
    ) {
        self.disabled_actions = disabled;
    }

    fn set_open(&mut self, open: Option<usize>, cx: &mut Context<Self>) {
        if self.open != open {
            self.open = open;
            self.item_focus = open.map(|_| 0);
            self.typeahead.clear();
            cx.notify();
        }
    }

    fn handle_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let menu_count = self.menus.len();
        if menu_count == 0 {
            return;
        }
        match key {
            "escape" => {
                self.set_open(None, cx);
                cx.stop_propagation();
            }
            "left" if self.open.is_some() => {
                let i = self.open.unwrap();
                self.set_open(Some(i.saturating_sub(1)), cx);
                cx.stop_propagation();
            }
            "right" if self.open.is_some() => {
                let i = self.open.unwrap();
                self.set_open(Some((i + 1).min(menu_count - 1)), cx);
                cx.stop_propagation();
            }
            "down" | "up" => {
                if self.open.is_none() {
                    self.set_open(Some(0), cx);
                    cx.stop_propagation();
                    return;
                }
                let names = menu_action_names(&self.menus[self.open.unwrap()].items);
                let delta = if key == "down" { 1 } else { -1 };
                self.item_focus = step_menu_item_focus(self.item_focus, names.len(), delta);
                cx.notify();
                cx.stop_propagation();
            }
            "enter" => {
                if let Some(menu_ix) = self.open
                    && let Some(item_ix) = self.item_focus
                    && let Some(action) = nth_menu_action(&self.menus[menu_ix].items, item_ix)
                {
                    window.dispatch_action(action, cx);
                    self.set_open(None, cx);
                }
                cx.stop_propagation();
            }
            _ if key.len() == 1 && key.chars().all(|c| c.is_alphanumeric()) => {
                if self.open.is_none() {
                    return;
                }
                self.typeahead.push_str(key);
                let names = menu_action_names(&self.menus[self.open.unwrap()].items);
                let from = self.item_focus.unwrap_or(0);
                if let Some(idx) = typeahead_menu_index(&names, from, &self.typeahead) {
                    self.item_focus = Some(idx);
                    cx.notify();
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// One row of a dropdown. Separators become hairlines; submenus flatten
    /// into a dim section header followed by their items (the current menu
    /// set has no submenus on Windows, but degrade gracefully); OS-managed
    /// system menus only exist on macOS and are skipped.
    fn render_item(
        &self,
        item: &OwnedMenuItem,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let s = Theme::scaled;
        match item {
            OwnedMenuItem::Separator => vec![
                div()
                    .my(px(s(3.0)))
                    .h(px(1.0))
                    .bg(Theme::border())
                    .into_any_element(),
            ],
            OwnedMenuItem::SystemMenu(_) => Vec::new(),
            OwnedMenuItem::Submenu(submenu) => {
                let mut out = vec![
                    div()
                        .px(px(s(10.0)))
                        .py(px(s(3.0)))
                        // A ratio, not a frozen 1px delta: `font_size() - 1.0`
                        // shrinks proportionally huge at small fonts and
                        // vanishes at large ones. `font_size_ratio` is the
                        // sanctioned "a hair under body" size.
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .text_color(Theme::text_dim())
                        .child(submenu.name.to_string())
                        .into_any_element(),
                ];
                for child in &submenu.items {
                    out.extend(self.render_item(child, window, cx));
                }
                out
            }
            OwnedMenuItem::Action { name, action, .. } => {
                if let Some(reason) = self.disabled_actions.get(action.name()).cloned() {
                    // Engine-gated action unavailable on the active
                    // connection: dimmed, inert, with the capability reason
                    // where the shortcut hint would sit.
                    return vec![
                        div()
                            .id(gpui::ElementId::Name(name.clone().into()))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(s(24.0)))
                            .h(px(Theme::line_height() + Theme::scaled(4.5)))
                            .px(px(s(10.0)))
                            .rounded(px(s(3.0)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(name.to_string())
                            .child(
                                div()
                                    .max_w(px(s(280.0)))
                                    .overflow_hidden()
                                    .text_size(px(Theme::font_size_ratio(0.92)))
                                    .text_color(Theme::text_dim())
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(reason.clone()),
                            )
                            // The inline hint truncates long reasons; the
                            // tooltip carries the whole sentence.
                            .app_tooltip(move |_window, cx| {
                                crate::app::tooltip_view(reason.clone(), cx)
                            })
                            .into_any_element(),
                    ];
                }
                let shortcut = window
                    .highest_precedence_binding_for_action(action.as_ref())
                    .map(|binding| {
                        let keystrokes: Vec<Keystroke> = binding
                            .keystrokes()
                            .iter()
                            .map(|k| k.inner().clone())
                            .collect();
                        format_keystrokes(&keystrokes)
                    });
                let action = action.boxed_clone();
                vec![
                    div()
                        .id(gpui::ElementId::Name(name.clone().into()))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(s(24.0)))
                        // Row grows with its own `font_size()` label instead of
                        // freezing at `s(24.0)` and clipping at a larger font.
                        .h(px(Theme::line_height() + Theme::scaled(4.5)))
                        .px(px(s(10.0)))
                        .rounded(px(s(3.0)))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        // Menu-item hover is the row-hover wash, matching kit
                        // menus (whose hover maps to `row_hover()`); the accent
                        // is reserved for the selected/primary state.
                        .hover(|st| st.bg(Theme::row_hover()))
                        .when(
                            self.focused_action_name().as_deref() == Some(name.as_ref()),
                            |d| d.bg(Theme::row_hover()),
                        )
                        .child(name.to_string())
                        .children(shortcut.map(|text| {
                            div()
                                // Shortcut hint, a hair under body (see the
                                // submenu header above) — proportional, not a
                                // frozen 1px delta.
                                .text_size(px(Theme::font_size_ratio(0.92)))
                                .text_color(Theme::text_dim())
                                .child(text)
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                window.dispatch_action(action.boxed_clone(), cx);
                                this.open = None;
                                cx.notify();
                            }),
                        )
                        .into_any_element(),
                ]
            }
        }
    }
}

impl WindowMenuBar {
    /// One window-control button (minimize / maximize / close), Windows
    /// style: full-height, fixed width, glyph centered. `window_control_area`
    /// gives native hit-testing (drag/snap-layouts) on Windows; the click
    /// handler covers Linux where everything is client-side.
    fn control_button(
        id: &'static str,
        glyph: &'static str,
        area: WindowControlArea,
        red: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .id(id)
            .window_control_area(area)
            .w(px(s(CONTROL_WIDTH)))
            .h_full()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            // Fixed-size window-control button: the glyph is sized to its box
            // (`s(CONTROL_WIDTH)` × `h_full` of the titlebar), so it stays
            // `scaled()` in step with that chrome rather than tracking the font.
            .text_size(px(s(12.0)))
            .map(|d| {
                if red {
                    // The close button: red background, canvas-colored glyph,
                    // brighter red on hover.
                    d.bg(Theme::error())
                        .text_color(Theme::ink_on(Theme::error()))
                        .hover(|st| st.bg(Theme::error().opacity(0.8)))
                } else {
                    d.text_color(Theme::text())
                        .hover(|st| st.bg(Theme::border()))
                }
            })
            .child(glyph)
            .on_click(on_click)
    }

    fn render_brand() -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .children(crate::icons::sqlly_icon().map(|icon| {
                img(icon)
                    .w(px(s(TITLEBAR_MARK_PX)))
                    .h(px(s(TITLEBAR_MARK_PX)))
                    .mt(px(s(TITLEBAR_MARK_NUDGE_Y)))
            }))
            .child(
                div()
                    .font(Theme::ui_font())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child("SQLLY"),
            )
    }

    fn render_layout_toggle(
        id: &'static str,
        icon: AppIcon,
        active: bool,
        tooltip: &'static str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let tip: gpui::SharedString = crate::localization::text(tooltip).into();
        div()
            .id(id)
            .w(px(s(28.0)))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor(gpui::CursorStyle::PointingHand)
            .text_color(layout_toggle_ink(active))
            .hover(|st| st.text_color(Theme::text()).bg(Theme::row_hover()))
            .app_tooltip(move |_window, cx| crate::app::tooltip_view(tip.clone(), cx))
            .on_click(on_click)
            .child(
                Icon::empty()
                    .path(icon.path())
                    .size(px(Theme::toolbar_icon_size())),
            )
    }

    fn render_layout_toggles(&self) -> impl IntoElement {
        let s = Theme::scaled;
        let layout = self.layout;
        div()
            .flex()
            .flex_row()
            .items_center()
            .h_full()
            .pr(px(s(2.0)))
            .gap(px(s(2.0)))
            .child(Self::render_layout_toggle(
                "title-toggle-sidebar",
                AppIcon::SidebarLeft,
                layout.sidebar,
                "shell.titlebar.toggle_sidebar_tooltip",
                |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::ToggleSidebar), cx);
                },
            ))
            .child(Self::render_layout_toggle(
                "title-toggle-ai",
                AppIcon::SidebarRight,
                layout.ai_drawer,
                "shell.titlebar.toggle_ai_tooltip",
                |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::ToggleAiPanel), cx);
                },
            ))
            .when(layout.ai_in_flight, |row| {
                row.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(4.0)))
                        .child(
                            div()
                                .w(px(s(6.0)))
                                .h(px(s(6.0)))
                                .rounded_full()
                                .bg(Theme::accent()),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.72)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text("shell.titlebar.ai_working")),
                        ),
                )
            })
            .child(Self::render_layout_toggle(
                "title-toggle-output",
                AppIcon::LayoutBottom,
                layout.log,
                "shell.titlebar.toggle_output_tooltip",
                |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::ToggleResultsPane), cx);
                },
            ))
    }
}

impl Render for WindowMenuBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let open = self.open;
        let menus = self.menus.clone();
        let is_maximized = window.is_maximized();

        // Titlebar and menu-bar-item heights derive from the interface font so
        // the whole bar grows with its labels (frozen `s(34)`/`s(24)` clipped
        // them once the font was enlarged). The dropdown anchor is expressed
        // from these same two heights — a menu item's bottom edge, `(bar +
        // item) / 2` — so it stays correct at any font size instead of assuming
        // the old fixed 34/24 geometry.
        let item_h = Theme::line_height() + s(4.5);
        let bar_h = item_h + s(10.0);
        let show_menus = cfg!(not(target_os = "macos"));
        let show_window_controls = cfg!(not(target_os = "macos"));

        div()
            .id("window-menu-bar")
            .flex()
            .flex_row()
            .items_center()
            // Explicit width: the bar is drawn from the frame cache, whose
            // first layout runs the root standalone rather than stretched by
            // the parent column; without this it would shrink to its content.
            .w_full()
            .h(px(bar_h))
            .flex_none()
            .bg(Theme::bg())
            .border_b_1()
            .border_color(Theme::border())
            .text_size(px(Theme::font_size()))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_menu_key(event, window, cx);
            }))
            .when(cfg!(target_os = "macos"), |d| {
                d.child(
                    div()
                        .w(px(macos_title_bar_leading_pad()))
                        .h_full()
                        .flex_none(),
                )
            })
            .when(show_menus, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .pl(px(s(10.0)))
                        .pr(px(s(6.0)))
                        .children(crate::icons::sqlly_icon().map(|icon| {
                            img(icon)
                                .w(px(s(TITLEBAR_MARK_PX)))
                                .h(px(s(TITLEBAR_MARK_PX)))
                                .mt(px(s(TITLEBAR_MARK_NUDGE_Y)))
                        })),
                )
            })
            .when(show_menus, |d| {
                d.children(menus.iter().enumerate().map(|(i, menu)| {
                    let is_open = open == Some(i);

                    let dropdown: Option<AnyElement> = if is_open {
                        let items: Vec<AnyElement> = menu
                            .items
                            .iter()
                            .flat_map(|item| self.render_item(item, window, cx))
                            .collect();
                        let dismiss = cx.entity();
                        Some(
                            div()
                                .absolute()
                                // Hang the dropdown from the menu item's bottom
                                // edge: an item of height `item_h` centered in a bar
                                // of height `bar_h` has its bottom at `(bar_h +
                                // item_h) / 2`. Derived from the same heights, so it
                                // tracks the font instead of assuming 34/24.
                                .top(px((bar_h + item_h) / 2.0))
                                .left(px(0.0))
                                .child(
                                    deferred(
                                        anchored().snap_to_window_with_margin(px(s(8.0))).child(
                                            div()
                                                .id(("menu-dropdown", i))
                                                .occlude()
                                                .min_w(px(s(220.0)))
                                                .max_h(px(s(480.0)))
                                                .overflow_y_scroll()
                                                .flex()
                                                .flex_col()
                                                .py(px(s(4.0)))
                                                .px(px(s(4.0)))
                                                .bg(Theme::popup_bg())
                                                .border_1()
                                                .border_color(Theme::popup_border())
                                                .rounded(px(s(6.0)))
                                                .children(items)
                                                .on_mouse_down_out(
                                                    move |_: &MouseDownEvent, _window, app| {
                                                        dismiss.update(app, |this, cx| {
                                                            if this.open.is_some() {
                                                                this.open = None;
                                                                cx.notify();
                                                            }
                                                        });
                                                    },
                                                ),
                                        ),
                                    )
                                    .with_priority(2),
                                )
                                .into_any_element(),
                        )
                    } else {
                        None
                    };

                    div()
                        .id(("menu-bar-item", i))
                        .relative()
                        .flex()
                        .flex_row()
                        .items_center()
                        .h(px(item_h))
                        .px(px(s(9.0)))
                        .rounded(px(s(4.0)))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .text_color(Theme::text())
                        .when(is_open, |d| d.bg(Theme::border()))
                        .when(!is_open, |d| {
                            d.hover(|st| st.bg(Theme::border().opacity(0.6)))
                        })
                        .child(menu.name.to_string())
                        // Click toggles off the render-time snapshot so the
                        // dropdown's capture-phase dismiss and this handler
                        // compose to "closed" instead of re-opening.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.set_open(if is_open { None } else { Some(i) }, cx);
                            }),
                        )
                        // Native menu-bar behavior: while any menu is open,
                        // sweeping across the titles switches menus.
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if this.open.is_some() && this.open != Some(i) {
                                this.set_open(Some(i), cx);
                            }
                        }))
                        .children(dropdown)
                }))
            })
            // Empty middle: the draggable caption area. On Windows the
            // control-area hitbox makes this native (drag, snap layouts,
            // double-click maximize); the handlers below cover Linux.
            .child(
                div()
                    .id("titlebar-drag")
                    .window_control_area(WindowControlArea::Drag)
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Self::render_brand())
                    // Drag-to-move and double-click-to-zoom have to share this
                    // area, and on Linux they conflict: `start_window_move`
                    // hands the compositor a pointer grab, and per its own
                    // docs "events may not be received during a move
                    // operation" — so the moment a move starts, the release
                    // and everything after it are gone and no click can ever
                    // complete. Hence: record the press, and only commit to a
                    // move once the pointer has actually travelled.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _window, _cx| {
                            this.caption_press = Some(ev.position);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, _cx| {
                        if !cfg!(target_os = "linux") || !ev.dragging() {
                            return;
                        }
                        let Some(origin) = this.caption_press else {
                            return;
                        };
                        let travelled = (ev.position.x - origin.x)
                            .abs()
                            .max((ev.position.y - origin.y).abs());
                        if travelled > px(DRAG_THRESHOLD) {
                            // Past the point of no return: the grab starts and
                            // no further events arrive, so drop the press.
                            this.caption_press = None;
                            window.start_window_move();
                        }
                    }))
                    .on_click(cx.listener(|this, ev: &ClickEvent, window, _cx| {
                        this.caption_press = None;
                        if ev.click_count() == 2 {
                            window.zoom_window();
                        }
                    })),
            )
            .child(self.render_layout_toggles())
            .when(show_window_controls, |d| {
                d.child(Self::control_button(
                    "win-minimize",
                    "\u{2500}", // ─
                    WindowControlArea::Min,
                    false,
                    |_, window, _| window.minimize_window(),
                ))
                .child(Self::control_button(
                    "win-maximize",
                    if is_maximized {
                        "\u{29C9}" // ⧉ restore (overlapping squares)
                    } else {
                        "\u{25A1}" // □ maximize
                    },
                    WindowControlArea::Max,
                    false,
                    |_, window, _| window.zoom_window(),
                ))
                .child(Self::control_button(
                    "win-close",
                    "\u{2715}", // ✕
                    WindowControlArea::Close,
                    true,
                    |_, window, cx| {
                        // In the browser this is the app's only window:
                        // removing it would blank the page, so close the tab
                        // the way Quit does.
                        #[cfg(target_arch = "wasm32")]
                        window.dispatch_action(Box::new(crate::app::Quit), cx);
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let _ = cx;
                            window.remove_window();
                        }
                    },
                ))
            })
    }
}

/// Format a key binding for display next to a menu item, Windows-style:
/// `Ctrl+Shift+P`; chords join with a space (`Ctrl+K Ctrl+C`).
fn format_keystrokes(keystrokes: &[Keystroke]) -> String {
    keystrokes
        .iter()
        .map(format_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_keystroke(stroke: &Keystroke) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if stroke.modifiers.control {
        parts.push("Ctrl");
    }
    if stroke.modifiers.alt {
        parts.push("Alt");
    }
    if stroke.modifiers.shift {
        parts.push("Shift");
    }
    if stroke.modifiers.platform {
        parts.push("Win");
    }
    let key = display_key(&stroke.key);
    let mut out = parts.join("+");
    if !out.is_empty() {
        out.push('+');
    }
    out.push_str(&key);
    out
}

/// Human-readable key name: single letters uppercase, common named keys get
/// their conventional Windows spelling, everything else capitalizes the
/// first letter (`f5` → `F5`, `pagedown` → `PageDown` via the map).
fn display_key(key: &str) -> String {
    match key {
        "escape" => "Esc".to_string(),
        "enter" => "Enter".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" => "Del".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "," => "Comma".to_string(),
        k if k.len() == 1 => k.to_uppercase(),
        k => {
            let mut chars = k.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_toggle_ink_is_dim_until_active() {
        // Compare against the live tokens so a palette swap can't silently
        // invert "active means full ink".
        assert_eq!(layout_toggle_ink(false), Theme::text_dim());
        assert_eq!(layout_toggle_ink(true), Theme::text());
    }

    #[test]
    fn titlebar_mark_is_16pt() {
        assert_eq!(TITLEBAR_MARK_PX, 16.0);
        assert_eq!(TITLEBAR_MARK_NUDGE_Y, 2.0);
    }

    #[test]
    fn macos_title_bar_leaves_room_for_traffic_lights() {
        assert!(macos_title_bar_leading_pad() >= 64.0);
        let pos = macos_traffic_light_position();
        assert!(f32::from(pos.x) >= 8.0);
        assert!(f32::from(pos.y) >= 8.0);
    }

    #[test]
    fn format_single_letter_with_modifiers() {
        let stroke = Keystroke::parse("ctrl-shift-p").unwrap();
        assert_eq!(format_keystroke(&stroke), "Ctrl+Shift+P");
    }

    #[test]
    fn format_function_key() {
        let stroke = Keystroke::parse("f5").unwrap();
        assert_eq!(format_keystroke(&stroke), "F5");
    }

    #[test]
    fn format_named_key() {
        let stroke = Keystroke::parse("ctrl-pagedown").unwrap();
        assert_eq!(format_keystroke(&stroke), "Ctrl+PageDown");
    }

    #[test]
    fn format_chord_joins_with_space() {
        let strokes = [
            Keystroke::parse("ctrl-k").unwrap(),
            Keystroke::parse("ctrl-c").unwrap(),
        ];
        assert_eq!(format_keystrokes(&strokes), "Ctrl+K Ctrl+C");
    }

    #[test]
    fn menu_keyboard_helpers_step_and_typeahead() {
        assert_eq!(step_menu_item_focus(None, 3, 1), Some(0));
        assert_eq!(step_menu_item_focus(Some(0), 3, 1), Some(1));
        assert_eq!(step_menu_item_focus(Some(2), 3, 1), Some(0));
        assert_eq!(step_menu_item_focus(Some(0), 3, -1), Some(2));
        let names = vec!["File".into(), "Edit".into(), "View".into()];
        assert_eq!(typeahead_menu_index(&names, 0, "e"), Some(1));
        assert_eq!(typeahead_menu_index(&names, 1, "v"), Some(2));
    }

    #[test]
    fn windows_menus_have_windows_names() {
        // Guard the Windows naming conventions: Exit under File, Options
        // under Tools, Maximize under Window, About at the bottom of Help,
        // and no macOS application/Services menu.
        let menus = crate::menu_bar::build_menus(false);
        if cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(
            menus.first().map(|m| m.name.to_string()).as_deref(),
            Some("File")
        );

        let names_of = |menu_name: &str| -> Vec<String> {
            menus
                .iter()
                .find(|m| m.name.as_ref() == menu_name)
                .expect("menu should exist")
                .items
                .iter()
                .filter_map(|i| match i {
                    gpui::MenuItem::Action { name, .. } => Some(name.to_string()),
                    _ => None,
                })
                .collect()
        };

        assert_eq!(names_of("File").last().map(String::as_str), Some("Exit"));
        assert!(names_of("Tools").contains(&"Options…".to_string()));
        assert!(names_of("Window").contains(&"Maximize".to_string()));
        assert!(!names_of("Window").contains(&"Bring All to Front".to_string()));
        assert_eq!(
            names_of("Help").last().map(String::as_str),
            Some("About SQLLY")
        );
    }
}
