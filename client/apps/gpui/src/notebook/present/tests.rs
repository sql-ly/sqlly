//! Presentation keyboard navigation, entering and leaving full screen, and
//! the zen and dense switches.

use gpui::{Focusable as _, TestAppContext};

use super::*;
use crate::notebook::dashboard::tests::{REVENUE, draw, open_fixture};

#[gpui::test]
fn presentation_walks_slides_from_the_keyboard_and_restores_on_esc(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    assert!(!cx.update(|window, _| window.is_fullscreen()));
    view.update_in(cx, |nb, window, cx| nb.start_presentation(None, window, cx));
    draw(&view, cx);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.surface()),
        Surface::Presentation
    );
    assert!(
        cx.update(|window, _| window.is_fullscreen()),
        "presenting fills the screen"
    );
    assert!(
        cx.update(|window, cx| view.read(cx).focus_handle(cx).is_focused(window)),
        "the presentation has keyboard focus"
    );
    assert_eq!(
        cx.update(|_, cx| cx.cursor_hide_mode()),
        CursorHideMode::OnTypingAndAction
    );

    let slide = |cx: &mut gpui::VisualTestContext| {
        view.read_with(cx, |nb, _| {
            (nb.current_slide(), nb.dashboard().unwrap().slide)
        })
    };
    assert_eq!(slide(cx), (0, SlideRef { section: 0, tab: 0 }));
    cx.simulate_keystrokes("right");
    assert_eq!(slide(cx), (1, SlideRef { section: 0, tab: 1 }));
    cx.simulate_keystrokes("space");
    assert_eq!(slide(cx), (2, SlideRef { section: 1, tab: 0 }));
    cx.simulate_keystrokes("right");
    assert_eq!(slide(cx).0, 2, "the last slide stays put");
    cx.simulate_keystrokes("left");
    assert_eq!(slide(cx).0, 1);
    cx.simulate_keystrokes("home");
    assert_eq!(slide(cx).0, 0);
    cx.simulate_keystrokes("end");
    assert_eq!(slide(cx).0, 2);
    cx.simulate_keystrokes("pageup");
    assert_eq!(slide(cx).0, 1);
    draw(&view, cx);

    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Notebook);
    assert!(
        !cx.update(|window, _| window.is_fullscreen()),
        "full screen ends with it"
    );
    draw(&view, cx);
}

#[gpui::test]
fn presenting_from_the_editor_returns_to_the_editor(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| nb.open_dashboard_editor(None, cx));
    view.update(cx, |nb, cx| {
        nb.show_slide(SlideRef { section: 0, tab: 1 }, cx)
    });
    view.update_in(cx, |nb, window, cx| nb.start_presentation(None, window, cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.current_slide()),
        1,
        "the presentation starts on the tab being edited"
    );
    assert!(view.read_with(cx, |nb, _| !nb.dashboard().unwrap().editing));
    view.update_in(cx, |nb, window, cx| nb.stop_presentation(window, cx));
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Dashboard);
    assert!(view.read_with(cx, |nb, _| nb.dashboard().unwrap().editing));
}

#[gpui::test]
fn a_whole_dashboard_presents_as_one_slide(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| {
        nb.change_layout_cell(
            "cell_dashboard",
            |spec| {
                spec.presentation.slides = SlideMode::Whole;
                true
            },
            cx,
        );
    });
    view.update_in(cx, |nb, window, cx| nb.start_presentation(None, window, cx));
    draw(&view, cx);
    cx.simulate_keystrokes("right");
    assert_eq!(view.read_with(cx, |nb, _| nb.current_slide()), 0);
    let plan = view.read_with(cx, |nb, _| nb.refresh_plan());
    assert!(
        plan.cells.contains(&"cell_shippers".to_string()),
        "every tab's tiles are on screen"
    );
}

#[gpui::test]
fn zen_hides_the_rails_and_dense_tightens_cells(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.present.cell_padding()),
        (12.0, 5.0)
    );
    view.update(cx, |nb, cx| {
        nb.toggle_zen(cx);
        nb.toggle_dense(cx);
    });
    draw(&view, cx);
    view.read_with(cx, |nb, _| {
        assert!(nb.present.zen);
        assert_eq!(nb.present.cell_padding(), (6.0, 2.0));
        assert_eq!(nb.surface(), Surface::Notebook);
    });
    view.update(cx, |nb, cx| {
        nb.command(crate::notebook::view::NotebookCommand::ToggleZen, cx)
    });
    assert!(!view.read_with(cx, |nb, _| nb.present.zen));
}

#[test]
fn presentations_force_only_the_theme_the_dashboard_asks_for() {
    assert_eq!(forced_theme(ThemeChoice::FollowApp), None);
    assert_eq!(
        forced_theme(ThemeChoice::Light),
        Some(crate::theme::ThemeKind::Light)
    );
    assert_eq!(
        forced_theme(ThemeChoice::Dark),
        Some(crate::theme::ThemeKind::Dark)
    );
}

#[test]
fn forced_themes_stack_and_keep_a_choice_made_in_settings() {
    use crate::theme::{ThemeKind, kind, set_kind};
    let _guard = crate::theme::THEME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    set_kind(ThemeKind::Light);
    let first = ForcedTheme::force(ThemeKind::Dark);
    assert_eq!(kind(), ThemeKind::Dark);
    let second = ForcedTheme::force(ThemeKind::Light);
    assert_eq!(kind(), ThemeKind::Light);
    drop(first);
    assert_eq!(
        kind(),
        ThemeKind::Light,
        "the newest presentation keeps its theme"
    );
    drop(second);
    assert_eq!(kind(), ThemeKind::Light, "back to the theme from before");

    set_kind(ThemeKind::System);
    let forced = ForcedTheme::force(ThemeKind::Dark);
    set_kind(ThemeKind::HighContrast);
    drop(forced);
    assert_eq!(
        kind(),
        ThemeKind::HighContrast,
        "a Settings choice made while presenting stays"
    );
    set_kind(ThemeKind::System);
}

#[gpui::test]
fn a_forced_theme_ends_with_esc_and_with_the_tab(cx: &mut TestAppContext) {
    use crate::theme::{ThemeKind, kind, set_kind};
    let _guard = crate::theme::THEME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    set_kind(ThemeKind::Light);
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| {
        nb.change_layout_cell(
            "cell_dashboard",
            |spec| {
                spec.theme = ThemeChoice::Dark;
                true
            },
            cx,
        );
    });
    view.update_in(cx, |nb, window, cx| nb.start_presentation(None, window, cx));
    assert_eq!(kind(), ThemeKind::Dark);
    cx.simulate_keystrokes("escape");
    assert_eq!(kind(), ThemeKind::Light, "Esc puts the theme back");

    view.update_in(cx, |nb, window, cx| nb.start_presentation(None, window, cx));
    assert_eq!(kind(), ThemeKind::Dark);
    // Closing the tab or its window while presenting drops the view and
    // the presentation state with it, never through Esc.
    let state = view.update(cx, |nb, _| std::mem::take(&mut nb.present));
    assert_eq!(kind(), ThemeKind::Dark);
    drop(state);
    assert_eq!(
        kind(),
        ThemeKind::Light,
        "closing the tab puts the theme back"
    );
    set_kind(ThemeKind::System);
}
