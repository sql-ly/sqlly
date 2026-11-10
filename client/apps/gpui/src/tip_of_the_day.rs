//! Tip of the Day: a short, rotating tip shown once per launch (after the
//! workbench is up) plus on demand from Help ▸ Tip of the Day… and the
//! command palette.
//!
//! The tip list is static ([`TIPS`]) and every tip describes a feature that
//! really ships — keep it that way when adding tips. The first entry is the fresh-
//! install tip and must stay the IntelliSense + feedback overview.
//!
//! Persistence lives in its own small file (`tip-of-the-day.json`, see
//! [`crate::preferences::tip_of_the_day_path`]) rather than a field on a
//! shared preferences struct: `show_on_startup` is the "Show tips on startup"
//! switch (also exposed in Settings ▸ Appearance ▸ Display) and `next_index`
//! is the tip the next startup will show. Showing a tip at startup advances
//! `next_index`, and Back/Next navigation re-points it at the tip after the
//! one last viewed, so the next launch always continues where you left off.
//!
//! GUI automation: set `SQLLY_SUPPRESS_TIP_OF_THE_DAY=1` in the launch
//! environment (or seed `tip-of-the-day.json` with `"show_on_startup":
//! false`) so a fresh fake-HOME sandbox doesn't open a modal over the
//! workbench.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Context, EventEmitter, IntoElement, Render, Styled, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

/// One tip: a short title and a one-paragraph body. `{modifier_key}` in the
/// body is a catalog placeholder filled with the platform's primary modifier
/// label (Cmd / Ctrl).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tip {
    pub title: &'static str,
    pub body: &'static str,
}

/// Environment switch that keeps the startup tip from opening (GUI
/// automation / fake-HOME sandboxes). Any non-empty value other than `0`
/// suppresses it; the Help menu entry still works.
// Only the desktop startup path claims the tip (not tests, not the browser).
#[cfg_attr(any(test, target_arch = "wasm32"), allow(dead_code))]
pub const SUPPRESS_ENV: &str = "SQLLY_SUPPRESS_TIP_OF_THE_DAY";

/// Every tip, in display order. Index 0 is what a fresh install shows first.
pub const TIPS: &[Tip] = &[
    Tip {
        title: "IntelliSense, and telling us what to fix",
        body: "As you type, SQLLY suggests the tables, columns, keywords, and even complete JOINs that fit where you are, drawn from your connected database. When a suggestion is wrong or missing, press F2 while the completion popup is open (or click its F2 hint) to send an Improve IntelliSense report — you choose exactly which schema names are shared, and reports become permanent regression tests. For anything else — a bug, an idea, a confusing screen — use Help ▸ Send Feedback… to send a screenshot you can mark up with arrows, with private areas blacked out before anything leaves your machine.",
    },
    Tip {
        title: "Every command, one shortcut away",
        body: "Press {modifier_key}+Shift+P to open the command palette: every menu command, searchable by name, category, or keyword, with its current keyboard shortcut shown beside it. Opening it with an empty search lists the commands you used most recently first.",
    },
    Tip {
        title: "Jump straight to any object",
        body: "Press {modifier_key}+P (Go to Object…) and type a few letters to fuzzy-search every connection, database, table, view, procedure, and function loaded in the explorer. Looking something up never changes which server your query tab is connected to.",
    },
    Tip {
        title: "JOINs written for you",
        body: "After a table name, autocomplete offers a complete JOIN … ON clause built from that table's foreign keys. Type i, l, r, or o first to get an INNER, LEFT, RIGHT, or FULL OUTER JOIN.",
    },
    Tip {
        title: "Hover to check the schema",
        body: "Rest the pointer on a table, column, procedure, or function name in the editor to see its columns and types, a column's type and nullability, or a routine's parameter list — without leaving your query.",
    },
    Tip {
        title: "Typed query parameters",
        body: "Add a line like `-- sqlly param: @customer_id int = 42` to a query and a parameter strip appears above the results with one typed field per parameter. On Run, each @name is replaced with a safely escaped literal for your engine, and recent values are offered as one-click chips.",
    },
    Tip {
        title: "Script variables",
        body: "A `-- sqlly set schema = sales` line defines a text placeholder: every ${schema} in the script is expanded before the query runs, so a repeated prefix, suffix, or cut-off date only needs changing in one place.",
    },
    Tip {
        title: "Run just the statement you're on",
        body: "Turn on the \"Run current statement only\" toggle in the query toolbar and Execute runs only the statement under the caret instead of the whole script. A selection still takes priority when you have one.",
    },
    Tip {
        title: "See the plan without running anything",
        body: "Query ▸ Explain Query (Estimated, No Execution) asks the optimizer how it would run your SQL — even an UPDATE or DELETE — without executing a single statement. The plan opens in the same pane as actual plans, clearly labeled Estimated.",
    },
    Tip {
        title: "Compare two runs of a query",
        body: "Result diff mode compares any two retained runs of a query tab — or the current run against a saved snapshot. Pick key columns to see inserted, deleted, and changed rows, and filter the change list to just what you care about.",
    },
    Tip {
        title: "Keep a result for later",
        body: "Save as Snapshot… captures the current result set with its query text and timestamp. Tools ▸ Result Snapshots… reopens it any time as a read-only tab, and a snapshot can be the baseline in the result diff view.",
    },
    Tip {
        title: "Build a WHERE clause from the grid",
        body: "Right-click a cell or row in the results to generate WHERE fragments from its actual value — equals, ranges, text patterns, IS NULL, IN, BETWEEN, or Match Row in WHERE Clause — quoted correctly for the engine the query ran against.",
    },
    Tip {
        title: "Follow a foreign key",
        body: "Right-click a cell that references another table and choose Open Referenced Row: SQLLY opens a new query tab selecting the parent row, on the same connection that produced the result.",
    },
    Tip {
        title: "Paste a list as IN (…)",
        body: "Copied a column from a spreadsheet or a list of ids from a ticket? Edit ▸ Paste as IN List splits the clipboard on commas, tabs, or newlines, quotes strings for your engine, leaves numbers bare, and inserts a ready-to-run IN list.",
    },
    Tip {
        title: "Snippets with tab stops",
        body: "Define snippets in Settings that expand a short prefix into a full SQL shape with numbered placeholders. Tab and Shift+Tab jump between the placeholders; Escape leaves the snippet.",
    },
    Tip {
        title: "Every query has a history",
        body: "Each query tab keeps a timeline of its own text, saved on every run and whenever you pause after editing. Open the History panel to restore an earlier version — your current text is saved first, and the restore is a single undo step.",
    },
    Tip {
        title: "Closed a tab by mistake?",
        body: "Press {modifier_key}+Shift+T (Window ▸ Reopen Closed Tab) to bring back the most recently closed query tab with its content, name, and connection.",
    },
    Tip {
        title: "Recent-first tab switching",
        body: "Ctrl+Tab cycles query tabs in the order you last used them, so one press returns to the tab you were just in. Back and Forward commands step through your tab visits like a browser.",
    },
    Tip {
        title: "Two views of one script",
        body: "View ▸ Split Editor Vertically (or Horizontally) shows two query tabs side by side. The Split Same Buffer commands give you two live views of the same query, each with its own cursor and scroll position.",
    },
    Tip {
        title: "Edit many lines at once",
        body: "Hold Alt (Option on a Mac) and drag, or press Shift+Alt+Up/Down, to select the same columns across several lines — each line gets its own caret, so typing, deleting, and pasting apply to all of them.",
    },
    Tip {
        title: "Production connections are guarded",
        body: "Mark a connection or its environment as production and SQLLY shows a warning banner, blocks data-changing statements until you unlock writes for a timed window, and asks you to confirm the exact SQL before each write.",
    },
    Tip {
        title: "Paste a connection string",
        body: "In the connection editor, paste a postgres://, mysql://, sqlserver://, JDBC, or ADO.NET-style connection string and SQLLY fills in the engine, host, port, database, user, and encryption options. Any password goes straight to your operating system's secure store.",
    },
    Tip {
        title: "Find objects faster in the explorer",
        body: "The explorer filter understands abbreviations like SOH for SalesOrderHeader and /regex/ patterns. Pin the objects you use most to the top of their server, and drag a table or column into the editor to insert its correctly quoted name.",
    },
    Tip {
        title: "Format on every run",
        body: "Switch on the Fmt toggle in the query toolbar and each Run formats the whole buffer first. The format is a single undo step, and it stays out of the way when you've selected text to run.",
    },
    Tip {
        title: "One-keystroke chores",
        body: "Quick actions are named SQL templates that use the selection, the word at the caret, or the current database. Assign one to a slot and give the slot a shortcut in Keyboard Shortcuts, and describing a table or counting its rows becomes a single keystroke.",
    },
    Tip {
        title: "Shortcuts that tell the truth",
        body: "Help ▸ SQLLY Help… is generated from the live command list and your own keybindings, so it always matches how the app behaves. Change any shortcut in Tools ▸ Open Keyboard Shortcuts….",
    },
];

/// The persisted Tip of the Day state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TipPreferences {
    /// "Show tips on startup".
    #[serde(default = "default_true")]
    pub show_on_startup: bool,
    /// Index of the tip the next startup shows (wrapped on read).
    #[serde(default)]
    pub next_index: usize,
}

fn default_true() -> bool {
    true
}

impl Default for TipPreferences {
    fn default() -> Self {
        Self {
            show_on_startup: true,
            next_index: 0,
        }
    }
}

/// Wrap any index into the tip list.
pub fn wrap(index: usize) -> usize {
    index % TIPS.len()
}

/// The tip after `index`, wrapping from the last tip back to the first.
pub fn next_index(index: usize) -> usize {
    wrap(wrap(index) + 1)
}

/// The tip before `index`, wrapping from the first tip to the last.
pub fn prev_index(index: usize) -> usize {
    let i = wrap(index);
    if i == 0 { TIPS.len() - 1 } else { i - 1 }
}

/// Title keys, parallel to [`TIPS`].
const TIP_TITLE_KEYS: &[&str] = &[
    "help.tip_of_the_day.tips.intellisense_title",
    "help.tip_of_the_day.tips.command_palette_title",
    "help.tip_of_the_day.tips.object_jump_title",
    "help.tip_of_the_day.tips.joins_title",
    "help.tip_of_the_day.tips.hover_title",
    "help.tip_of_the_day.tips.parameters_title",
    "help.tip_of_the_day.tips.variables_title",
    "help.tip_of_the_day.tips.current_statement_title",
    "help.tip_of_the_day.tips.estimated_plan_title",
    "help.tip_of_the_day.tips.result_diff_title",
    "help.tip_of_the_day.tips.snapshot_title",
    "help.tip_of_the_day.tips.where_clause_title",
    "help.tip_of_the_day.tips.foreign_key_title",
    "help.tip_of_the_day.tips.paste_in_title",
    "help.tip_of_the_day.tips.snippets_title",
    "help.tip_of_the_day.tips.history_title",
    "help.tip_of_the_day.tips.reopen_tab_title",
    "help.tip_of_the_day.tips.tab_switching_title",
    "help.tip_of_the_day.tips.split_editor_title",
    "help.tip_of_the_day.tips.multicursor_title",
    "help.tip_of_the_day.tips.production_title",
    "help.tip_of_the_day.tips.connection_string_title",
    "help.tip_of_the_day.tips.explorer_filter_title",
    "help.tip_of_the_day.tips.format_on_run_title",
    "help.tip_of_the_day.tips.quick_actions_title",
    "help.tip_of_the_day.tips.shortcut_help_title",
];

/// Body keys, parallel to [`TIP_TITLE_KEYS`] and [`TIPS`].
const TIP_BODY_KEYS: &[&str] = &[
    "help.tip_of_the_day.tips.intellisense_body",
    "help.tip_of_the_day.tips.command_palette_body",
    "help.tip_of_the_day.tips.object_jump_body",
    "help.tip_of_the_day.tips.joins_body",
    "help.tip_of_the_day.tips.hover_body",
    "help.tip_of_the_day.tips.parameters_body",
    "help.tip_of_the_day.tips.variables_body",
    "help.tip_of_the_day.tips.current_statement_body",
    "help.tip_of_the_day.tips.estimated_plan_body",
    "help.tip_of_the_day.tips.result_diff_body",
    "help.tip_of_the_day.tips.snapshot_body",
    "help.tip_of_the_day.tips.where_clause_body",
    "help.tip_of_the_day.tips.foreign_key_body",
    "help.tip_of_the_day.tips.paste_in_body",
    "help.tip_of_the_day.tips.snippets_body",
    "help.tip_of_the_day.tips.history_body",
    "help.tip_of_the_day.tips.reopen_tab_body",
    "help.tip_of_the_day.tips.tab_switching_body",
    "help.tip_of_the_day.tips.split_editor_body",
    "help.tip_of_the_day.tips.multicursor_body",
    "help.tip_of_the_day.tips.production_body",
    "help.tip_of_the_day.tips.connection_string_body",
    "help.tip_of_the_day.tips.explorer_filter_body",
    "help.tip_of_the_day.tips.format_on_run_body",
    "help.tip_of_the_day.tips.quick_actions_body",
    "help.tip_of_the_day.tips.shortcut_help_body",
];

fn tip_index(tip: &Tip) -> usize {
    TIPS.iter()
        .position(|candidate| candidate == tip)
        .expect("tip belongs to the registry")
}

pub fn render_title(tip: &Tip) -> String {
    crate::localization::text(TIP_TITLE_KEYS[tip_index(tip)])
}

/// Tip bodies whose catalog text has a `{modifier_key}` placeholder.
const MODIFIER_TIP_BODY_KEYS: &[&str] = &[
    "help.tip_of_the_day.tips.command_palette_body",
    "help.tip_of_the_day.tips.object_jump_body",
    "help.tip_of_the_day.tips.reopen_tab_body",
];

/// Body text with `{modifier_key}` filled with the platform's primary modifier.
pub fn render_body(tip: &Tip) -> String {
    let key = TIP_BODY_KEYS[tip_index(tip)];
    if MODIFIER_TIP_BODY_KEYS.contains(&key) {
        crate::localization::format(key, &[("modifier_key", crate::keybindings::PRIMARY_LABEL)])
    } else {
        crate::localization::text(key)
    }
}

impl TipPreferences {
    /// Startup decision: when tips are on, return the tip to show and advance
    /// `next_index` past it (the caller persists). `None` when disabled.
    pub fn take_startup_tip(&mut self) -> Option<usize> {
        if !self.show_on_startup {
            return None;
        }
        let index = wrap(self.next_index);
        self.next_index = next_index(index);
        Some(index)
    }

    /// Record that the user navigated to `index`: the next startup continues
    /// with the tip after it.
    pub fn record_viewed(&mut self, index: usize) {
        self.next_index = next_index(index);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_from_disk() -> Self {
        std::fs::read_to_string(crate::preferences::tip_of_the_day_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    #[cfg(target_arch = "wasm32")]
    fn load_from_disk() -> Self {
        crate::preferences::wasm_load_json(FILE_NAME)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save_to_disk(&self) {
        if let Ok(text) = crate::config_version::to_string_pretty(self) {
            let _ = std::fs::write(crate::preferences::tip_of_the_day_path(), text);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn save_to_disk(&self) {
        crate::preferences::wasm_save_json(FILE_NAME, self);
    }
}

/// File name under the app's data directory (localStorage key on wasm).
pub(crate) const FILE_NAME: &str = "tip-of-the-day.json";

/// Process-wide cache so the Settings checkbox and the dialog agree without
/// a disk read per render.
fn store() -> &'static Mutex<TipPreferences> {
    static STORE: OnceLock<Mutex<TipPreferences>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(TipPreferences::load_from_disk()))
}

/// Current Tip of the Day preferences.
pub fn prefs() -> TipPreferences {
    store().lock().clone()
}

/// Mutate and persist the preferences.
pub fn update_prefs(f: impl FnOnce(&mut TipPreferences)) {
    let mut guard = store().lock();
    f(&mut guard);
    guard.save_to_disk();
}

/// Whether the startup tip is suppressed by the environment.
pub fn startup_suppressed_by_env(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some(v) if !v.is_empty() && v != "0")
}

/// Claim the once-per-process startup tip. Returns the index to show, or
/// `None` when tips are off, suppressed, or a window already showed one
/// (File ▸ New Window must not re-show it).
// Only the desktop startup path claims the tip (not tests, not the browser).
#[cfg_attr(any(test, target_arch = "wasm32"), allow(dead_code))]
pub fn claim_startup_tip() -> Option<usize> {
    static CLAIMED: AtomicBool = AtomicBool::new(false);
    if CLAIMED.swap(true, Ordering::SeqCst) {
        return None;
    }
    if startup_suppressed_by_env(std::env::var(SUPPRESS_ENV).ok().as_deref()) {
        return None;
    }
    let mut shown = None;
    update_prefs(|p| shown = p.take_startup_tip());
    shown
}

// ------------------------------------------------------------------------
// Dialog
// ------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum TipOfTheDayEvent {
    Closed,
}

/// The Tip of the Day dialog: tip text, Back/Next, "Show tips on startup",
/// and Close.
pub struct TipOfTheDayDialog {
    index: usize,
    frame: DialogFrame,
}

impl EventEmitter<TipOfTheDayEvent> for TipOfTheDayDialog {}

impl TipOfTheDayDialog {
    /// A dialog showing tip `index` (wrapped). Opening it manually does not
    /// advance the startup position; navigating does.
    pub fn new(index: usize) -> Self {
        Self {
            index: wrap(index),
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
        }
    }

    /// The dialog opened from Help: shows the tip the next startup would.
    pub fn for_menu() -> Self {
        Self::new(prefs().next_index)
    }

    fn go(&mut self, index: usize, cx: &mut Context<Self>) {
        self.index = wrap(index);
        let viewed = self.index;
        update_prefs(|p| p.record_viewed(viewed));
        cx.notify();
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        self.go(next_index(self.index), cx);
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.go(prev_index(self.index), cx);
    }
}

impl Render for TipOfTheDayDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let tip = TIPS[self.index];
        let show_on_startup = prefs().show_on_startup;
        let counter = crate::localization::format(
            "help.tip_of_the_day.counter",
            &[
                ("current", &(self.index + 1).to_string()),
                ("total", &TIPS.len().to_string()),
            ],
        );

        Dialog::new("tip-of-the-day", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "help.tip_of_the_day.dialog_title",
            ))
            .easy_close(true)
            .on_close(|_this, _window, cx| cx.emit(TipOfTheDayEvent::Closed))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(counter),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(1.15)))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .child(render_title(&tip)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .line_height(gpui::relative(1.4))
                            .child(render_body(&tip)),
                    ),
            )
            .leading_button(
                Checkbox::new("tip-of-the-day-show-on-startup")
                    .checked(show_on_startup)
                    .label(crate::localization::text(
                        "help.tip_of_the_day.show_on_startup",
                    ))
                    .on_click(cx.listener(|_this, checked: &bool, _window, cx| {
                        let checked = *checked;
                        update_prefs(|p| p.show_on_startup = checked);
                        cx.notify();
                    })),
            )
            .button(
                Button::new("tip-of-the-day-back")
                    .label(crate::localization::text("help.tip_of_the_day.back_button"))
                    .on_click(cx.listener(|this, _, _window, cx| this.back(cx))),
            )
            .button(
                Button::new("tip-of-the-day-next")
                    .primary()
                    .label(crate::localization::text("help.tip_of_the_day.next_button"))
                    .on_click(cx.listener(|this, _, _window, cx| this.next(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_list_is_substantial_and_well_formed() {
        assert!(
            (20..=30).contains(&TIPS.len()),
            "expected 20-30 tips, got {}",
            TIPS.len()
        );
        for tip in TIPS {
            assert!(!tip.title.trim().is_empty());
            assert!(!tip.body.trim().is_empty());
        }
        assert_eq!(TIP_TITLE_KEYS.len(), TIPS.len());
        assert_eq!(TIP_BODY_KEYS.len(), TIPS.len());
        let mut titles: Vec<_> = TIPS.iter().map(|t| t.title).collect();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), TIPS.len(), "tip titles must be unique");
    }

    #[test]
    fn first_run_tip_is_intellisense_and_feedback_overview() {
        let mut prefs = TipPreferences::default();
        let first = prefs.take_startup_tip().expect("tips on by default");
        assert_eq!(first, 0);
        let tip = TIPS[first];
        assert!(tip.title.contains("IntelliSense"));
        assert!(tip.body.contains("F2"));
        assert!(tip.body.contains("Improve IntelliSense"));
        assert!(tip.body.contains("Send Feedback"));
    }

    #[test]
    fn startup_advances_and_wraps() {
        let mut prefs = TipPreferences::default();
        let shown: Vec<usize> = (0..TIPS.len() + 2)
            .map(|_| prefs.take_startup_tip().unwrap())
            .collect();
        let expected: Vec<usize> = (0..TIPS.len()).chain([0, 1]).collect();
        assert_eq!(shown, expected);
    }

    #[test]
    fn out_of_range_persisted_index_is_wrapped() {
        let mut prefs = TipPreferences {
            show_on_startup: true,
            next_index: TIPS.len() * 3 + 2,
        };
        assert_eq!(prefs.take_startup_tip(), Some(2));
        assert_eq!(prefs.next_index, 3);
    }

    #[test]
    fn disabled_flag_is_respected_and_keeps_position() {
        let mut prefs = TipPreferences {
            show_on_startup: false,
            next_index: 5,
        };
        assert_eq!(prefs.take_startup_tip(), None);
        assert_eq!(prefs.next_index, 5, "a skipped startup must not advance");
    }

    #[test]
    fn navigation_wraps_both_ways_and_updates_next_startup() {
        assert_eq!(next_index(TIPS.len() - 1), 0);
        assert_eq!(prev_index(0), TIPS.len() - 1);
        assert_eq!(next_index(3), 4);
        assert_eq!(prev_index(3), 2);

        let mut prefs = TipPreferences::default();
        prefs.record_viewed(TIPS.len() - 1);
        assert_eq!(prefs.take_startup_tip(), Some(0));
        prefs.record_viewed(7);
        assert_eq!(prefs.take_startup_tip(), Some(8));
    }

    #[test]
    fn preferences_round_trip_and_default_missing_fields() {
        let prefs = TipPreferences {
            show_on_startup: false,
            next_index: 9,
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: TipPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prefs);

        // A file written with only the switch (e.g. a harness seed) keeps the
        // other field at its default, and an empty object means "tips on".
        let seeded: TipPreferences = serde_json::from_str(r#"{"show_on_startup": false}"#).unwrap();
        assert!(!seeded.show_on_startup);
        assert_eq!(seeded.next_index, 0);
        let empty: TipPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, TipPreferences::default());
    }

    #[test]
    fn env_suppression_values() {
        assert!(!startup_suppressed_by_env(None));
        assert!(!startup_suppressed_by_env(Some("")));
        assert!(!startup_suppressed_by_env(Some("0")));
        assert!(startup_suppressed_by_env(Some("1")));
        assert!(startup_suppressed_by_env(Some("true")));
    }

    #[test]
    fn modifier_placeholder_is_resolved() {
        for tip in TIPS {
            assert!(!render_body(tip).contains("{modifier_key}"));
        }
        let palette = TIPS
            .iter()
            .find(|t| t.body.contains("{modifier_key}+Shift+P"))
            .expect("command palette tip");
        assert!(render_body(palette).contains(crate::keybindings::PRIMARY_LABEL));
    }

    #[test]
    fn dialog_index_wraps() {
        let d = TipOfTheDayDialog::new(TIPS.len() + 1);
        assert_eq!(d.index, 1);
    }
}
