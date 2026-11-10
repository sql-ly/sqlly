//! Gestures pane and gesture-preference persistence.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // Gestures
    // ============================================================

    pub(super) fn render_gestures(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = load_gesture_prefs();

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.gestures.heading.gestures",
            )),
            self.intro(&crate::localization::text(
                "settings.gestures.intro.pane_intro",
            )),
            self.section_header(&crate::localization::text(
                "settings.gestures.section_header.results",
            )),
            self.checkbox(
                "two_finger_results",
                &crate::localization::text("settings.gestures.checkbox.two_finger_result_scroll"),
                prefs.results_two_finger_scroll_enabled,
                cx,
                Self::toggle_two_finger_results,
            ),
            self.labeled_row(
                &crate::localization::text(
                    "settings.gestures.labeled_row.multi_result_scroll_modifier",
                ),
                self.enum_select(
                    "results_modifier",
                    prefs.result_container_scroll_modifier,
                    scroll_modifier_presets()
                        .into_iter()
                        .map(|(_, m)| m)
                        .collect(),
                    |m| m.display_title(),
                    |this, m, cx| this.set_gesture_modifier(m, cx),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.gestures.hint.stacked_scroll_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.gestures.section_header.query",
            )),
            self.labeled_row(
                &crate::localization::text("settings.gestures.labeled_row.two_finger_query_swap"),
                self.enum_select(
                    "query_swap_dir",
                    prefs.query_swap_direction,
                    vec![
                        QuerySwapGestureDirection::Disabled,
                        QuerySwapGestureDirection::Vertical,
                        QuerySwapGestureDirection::Horizontal,
                    ],
                    |d| query_swap_label(d).to_string(),
                    |this, d, cx| this.set_query_swap_direction(d, cx),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.gestures.hint.query_swap_hint",
            )),
        ])
    }

    // ---- Gesture pane actions ----

    pub(super) fn update_gesture_prefs<F: FnOnce(&mut GesturePreferences)>(
        &self,
        cx: &mut Context<Self>,
        f: F,
    ) {
        let path = gesture_prefs_path();
        let mut prefs = crate::gesture_prefs::load_preferences(&path);
        f(&mut prefs);
        let _ = crate::gesture_prefs::save_preferences(&prefs, &path);
        cx.notify();
    }

    pub(super) fn set_gesture_modifier(
        &mut self,
        m: ResultContainerScrollModifier,
        cx: &mut Context<Self>,
    ) {
        self.update_gesture_prefs(cx, |p| p.result_container_scroll_modifier = m);
    }

    pub(super) fn set_query_swap_direction(
        &mut self,
        d: QuerySwapGestureDirection,
        cx: &mut Context<Self>,
    ) {
        self.update_gesture_prefs(cx, |p| p.query_swap_direction = d);
    }

    pub(super) fn toggle_two_finger_results(&mut self, cx: &mut Context<Self>) {
        let path = gesture_prefs_path();
        let mut prefs = crate::gesture_prefs::load_preferences(&path);
        prefs.results_two_finger_scroll_enabled = !prefs.results_two_finger_scroll_enabled;
        let _ = crate::gesture_prefs::save_preferences(&prefs, &path);
        cx.notify();
    }
}
