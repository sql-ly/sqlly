//! Output-panel tab chrome and empty-state helpers, split from the flat
//! `results` module.

use super::*;

/// Unified bottom output-panel tab (Phase 9) — everything shares one strip so
/// the editor stays on screen. Results, Messages, and History are always
/// present; Plan, DDL, Preview SQL, Diff, and Statistics appear only when they
/// have content (see [`TabVisibility`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputPanelTab {
    #[default]
    Results,
    Messages,
    Plan,
    History,
    Ddl,
    PreviewSql,
    Diff,
    Statistics,
}

/// Which of the conditional output tabs currently have something to show.
///
/// `Plan` and `Ddl` used to be permanent members of the tab strip and sat
/// there empty for every run that had neither. They are conditional now: the
/// Plan tab appears for a run that asked for an execution plan, and the DDL
/// tab appears when a DDL viewer has been bound to the pane.
#[derive(Debug, Clone, Copy, Default)]
pub struct TabVisibility {
    /// Whether the output chrome is up at all (false before the first run).
    pub chrome_visible: bool,
    pub plan: bool,
    pub ddl: bool,
    pub preview_sql: bool,
    pub diff: bool,
    pub stats: bool,
}

impl TabVisibility {
    /// Chrome up with only the always-present tabs — the shape most tests want.
    #[cfg(test)]
    pub fn chrome_only() -> Self {
        Self {
            chrome_visible: true,
            ..Self::default()
        }
    }
}

impl OutputPanelTab {
    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Results => "results.output_tabs.results",
            Self::Messages => "results.output_tabs.messages",
            Self::Plan => "results.output_tabs.plan",
            Self::History => "results.output_tabs.history",
            Self::Ddl => "results.output_tabs.ddl",
            Self::PreviewSql => "results.output_tabs.preview_sql",
            Self::Diff => "results.output_tabs.diff",
            Self::Statistics => "results.output_tabs.statistics",
        })
    }

    /// Cmd+1…5 (and Cmd+6 for Diff when visible). Statistics has no numbered
    /// shortcut.
    pub fn shortcut_index(self) -> Option<u8> {
        match self {
            Self::Results => Some(1),
            Self::Messages => Some(2),
            Self::Plan => Some(3),
            Self::History => Some(4),
            Self::Ddl => Some(5),
            Self::Diff => Some(6),
            Self::Statistics => None,
            Self::PreviewSql => None,
        }
    }

    pub fn command_id(self) -> &'static str {
        match self {
            Self::Results => "view.selectOutputResults",
            Self::Messages => "view.selectOutputMessages",
            Self::Plan => "view.selectOutputPlan",
            Self::History => "view.selectOutputHistory",
            Self::Ddl => "view.selectOutputDdl",
            Self::PreviewSql => "view.selectOutputPreviewSql",
            Self::Diff => "view.selectOutputDiff",
            Self::Statistics => "view.selectOutputStatistics",
        }
    }

    /// Tabs that are always present once the output chrome is up. Everything
    /// else — Plan, DDL, Preview SQL, Diff, Statistics — appears only when it
    /// has something to show; see [`TabVisibility`]. Test-only: `visible_tabs`
    /// interleaves the conditional tabs rather than appending to this list, so
    /// this exists to pin the always-present set.
    #[cfg(test)]
    pub fn core_tabs() -> [Self; 3] {
        [Self::Results, Self::Messages, Self::History]
    }

    /// Visible output chrome tabs. Empty before the first run (Phase 8 12b).
    pub fn visible_tabs(visibility: TabVisibility) -> Vec<Self> {
        if !visibility.chrome_visible {
            return Vec::new();
        }
        let mut tabs = vec![Self::Results, Self::Messages];
        // Plan sits next to Results because that is where it reads; it is
        // present only for a run that actually asked for an execution plan.
        if visibility.plan {
            tabs.push(Self::Plan);
        }
        tabs.push(Self::History);
        if visibility.ddl {
            tabs.push(Self::Ddl);
        }
        if visibility.preview_sql {
            tabs.push(Self::PreviewSql);
        }
        if visibility.diff {
            tabs.push(Self::Diff);
        }
        if visibility.stats {
            tabs.push(Self::Statistics);
        }
        tabs
    }

    pub(super) fn tooltip(self) -> String {
        let hint = crate::keybindings::shortcut_hint(self.command_id());
        if !hint.is_empty() {
            crate::localization::format(
                "results.output_tabs.tooltip_with_shortcut",
                &[("tab_name", &self.label()), ("shortcut", &hint)],
            )
        } else if let Some(i) = self.shortcut_index() {
            crate::localization::format(
                "results.output_tabs.tooltip_with_index",
                &[("tab_name", &self.label()), ("shortcut", &i.to_string())],
            )
        } else {
            self.label()
        }
    }
}

/// Compact superscript digits for results chrome tab counts (`Messages³`).
pub(super) fn tab_count_superscript(count: usize) -> String {
    const DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    count
        .to_string()
        .chars()
        .map(|c| DIGITS[(c as u8 - b'0') as usize])
        .collect()
}

/// Sans tab caption: label plus a count superscript when `count > 0`.
pub(super) fn chrome_tab_caption(label: impl AsRef<str>, count: usize) -> String {
    let label = label.as_ref();
    if count == 0 {
        label.to_string()
    } else {
        format!("{label}{}", tab_count_superscript(count))
    }
}

/// One row in the results empty-state shortcut list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EmptyStateShortcut {
    pub(super) label: String,
    pub(super) keycap: String,
}

pub(super) fn platform_primary_hint(rest: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("Cmd+{rest}")
    } else {
        format!("Ctrl+{rest}")
    }
}

/// Shortcut rows shown when the results pane has nothing to display.
/// Bindings come from the live keybinding snapshot so a rebind rewrites the table.
pub(super) fn empty_state_shortcuts() -> Vec<EmptyStateShortcut> {
    let run = crate::keybindings::shortcut_hint("query.execute");
    let new_query = crate::keybindings::shortcut_hint("file.newQuery");
    let palette = crate::keybindings::shortcut_hint("tools.commandPalette");
    vec![
        EmptyStateShortcut {
            label: crate::localization::text("results.empty_state.run_query_shortcut"),
            keycap: if run.is_empty() {
                platform_primary_hint("Enter")
            } else {
                run
            },
        },
        EmptyStateShortcut {
            label: crate::localization::text("results.empty_state.new_query_shortcut"),
            keycap: if new_query.is_empty() {
                platform_primary_hint("N")
            } else {
                new_query
            },
        },
        EmptyStateShortcut {
            label: crate::localization::text("results.empty_state.command_palette_shortcut"),
            keycap: if palette.is_empty() {
                platform_primary_hint("Shift+P")
            } else {
                palette
            },
        },
        EmptyStateShortcut {
            label: crate::localization::text("results.empty_state.filter_explorer_shortcut"),
            keycap: platform_primary_hint("Shift+F"),
        },
    ]
}

/// Empty-state shortcuts render as mono keycap chips, matching the welcome screen.
#[cfg(test)]
pub(super) fn shortcut_uses_keycap_chrome() -> bool {
    true
}

/// Right-aligned keycap chips for one shortcut row: each comma-separated
/// binding ("Cmd+Enter, F5") becomes its own kit `Kbd` chip, same chrome as
/// the welcome screen's shortcut table (shared helper).
pub(super) fn render_shortcut_hint(text: &str) -> gpui::Div {
    crate::welcome::render_keycap_chips(text)
}

pub(super) fn render_empty_state_shortcuts() -> gpui::AnyElement {
    let s = Theme::scaled;
    div()
        .w(px(s(240.0)))
        .flex()
        .flex_col()
        .gap(px(s(6.0)))
        .children(empty_state_shortcuts().into_iter().map(|row| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(s(16.0)))
                .child(
                    div()
                        .font(Theme::ui_font())
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(row.label),
                )
                .child(render_shortcut_hint(&row.keycap))
        }))
        .into_any_element()
}

/// Shared chrome for Results / Messages / Diff / Plan / per-set tabs: sans text, amber underline.
pub(super) fn chrome_tab_shell(
    id: impl Into<ElementId>,
    is_active: bool,
) -> gpui::Stateful<gpui::Div> {
    let s = Theme::scaled;
    div()
        .id(id)
        .px(px(s(12.0)))
        .h_full()
        .flex()
        .flex_col()
        .justify_end()
        .cursor_pointer()
        .font(Theme::ui_font())
        .text_size(px(Theme::font_size()))
        .font_weight(if is_active {
            FontWeight::MEDIUM
        } else {
            FontWeight::NORMAL
        })
        .text_color(if is_active {
            Theme::text()
        } else {
            Theme::text_dim()
        })
        .hover(|st| st.text_color(Theme::text()))
}

pub(super) fn chrome_tab_underline(is_active: bool) -> gpui::Div {
    let s = Theme::scaled;
    div().h(px(s(2.0))).w_full().bg(if is_active {
        Theme::underline_accent()
    } else {
        gpui::transparent_black()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_tab_labels_match_the_source_catalog() {
        for (tab, expected) in [
            (OutputPanelTab::Results, "Results"),
            (OutputPanelTab::Messages, "Messages"),
            (OutputPanelTab::Plan, "Plan"),
            (OutputPanelTab::History, "History"),
            (OutputPanelTab::Ddl, "DDL"),
            (OutputPanelTab::PreviewSql, "Preview SQL"),
            (OutputPanelTab::Diff, "Diff"),
            (OutputPanelTab::Statistics, "Statistics"),
        ] {
            assert_eq!(tab.label(), expected);
        }
    }
}
