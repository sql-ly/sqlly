//! Query History Browser — a modal overlay over the local execution log
//! (`query_history`): live search across SQL, server, and database, a pinned
//! section for keeper snippets, and script-to-editor for the full statement.
//!
//! The dialog reads local files only, so unlike the admin tool dialogs it
//! takes no `ToolDialogContext`; it still speaks [`ToolDialogEvent`] so the
//! host wires it exactly like the other overlay dialogs.

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent,
    ParentElement, Render, ScrollHandle, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::query_history::{self, HistoryCursor, HistoryEntry};
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogEvent;

/// How many log entries are loaded for searching.
const READ_LIMIT: usize = 1000;
/// Hard cap on rendered Recent rows; searching narrows past it.
const RECENT_DISPLAY_CAP: usize = 200;
/// Character budget of a row's one-line SQL preview.
const PREVIEW_MAX_CHARS: usize = 120;

/// First non-empty line of `sql`, truncated to [`PREVIEW_MAX_CHARS`] chars
/// with an ellipsis.
fn sql_preview_line(sql: &str) -> String {
    let first = sql
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut preview: String = first.chars().take(PREVIEW_MAX_CHARS).collect();
    if first.chars().count() > PREVIEW_MAX_CHARS {
        preview.push('…');
    }
    preview
}

/// Maps pinned history snippets to `.sql` files under `dir/Pinned`, disambiguating
/// duplicate stems the same way Save to Library does.
#[cfg(not(target_arch = "wasm32"))]
pub fn pins_to_library_files(pins: &[HistoryEntry], dir: &Path) -> Vec<(PathBuf, String)> {
    use crate::sql_library_model::{pin_to_library_file, unique_file_name};
    use std::collections::HashSet;

    let pinned = dir.join("Pinned");
    let mut reserved = HashSet::new();
    let mut out = Vec::new();
    for pin in pins {
        let (stem, contents) = pin_to_library_file(pin);
        let mut path = unique_file_name(&pinned, &stem);
        if reserved.contains(&path) {
            let mut n = 2u32;
            loop {
                let candidate = pinned.join(format!("{stem} ({n}).sql"));
                if !candidate.exists() && !reserved.contains(&candidate) {
                    path = candidate;
                    break;
                }
                n = n.saturating_add(1);
                if n == u32::MAX {
                    path = candidate;
                    break;
                }
            }
        }
        reserved.insert(path.clone());
        out.push((path, contents));
    }
    out
}

/// Relative-ish label for a log timestamp ("just now", "5m ago", "3h ago",
/// "2d ago", then the plain date). Unparseable input passes through verbatim;
/// clock skew into the future falls back to the date.
fn timestamp_label(timestamp: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%SZ") else {
        return timestamp.to_string();
    };
    let then = parsed.and_utc();
    let secs = now.signed_duration_since(then).num_seconds();
    match secs {
        s if (0..60).contains(&s) => {
            crate::localization::text("query.history_browser.age_just_now")
        }
        s if (60..3_600).contains(&s) => crate::localization::format(
            "query.history_browser.age_minutes",
            &[("count", &(s / 60).to_string())],
        ),
        s if (3_600..86_400).contains(&s) => crate::localization::format(
            "query.history_browser.age_hours",
            &[("count", &(s / 3_600).to_string())],
        ),
        s if (86_400..7 * 86_400).contains(&s) => crate::localization::format(
            "query.history_browser.age_days",
            &[("count", &(s / 86_400).to_string())],
        ),
        _ => then.format("%Y-%m-%d").to_string(),
    }
}

/// One rendered list row: `pinned` marks rows in the Pinned section (an entry
/// that is both pinned and recent appears in both sections).
struct VisibleRow {
    entry: HistoryEntry,
    pinned: bool,
}

impl EventEmitter<ToolDialogEvent> for QueryHistoryBrowser {}

pub struct QueryHistoryBrowser {
    /// Recent log entries, newest first.
    entries: Vec<HistoryEntry>,
    pins: Vec<HistoryEntry>,
    retention_removed: usize,
    filter: String,
    filter_input: Entity<InputState>,
    /// Tag editor for the selected pinned snippet; synced on selection
    /// changes with an echo guard (shared with the name and description
    /// editors below).
    tags_input: Entity<InputState>,
    /// Name editor for the selected pinned snippet — a named pin is a saved
    /// query and its rows lead with the name.
    name_input: Entity<InputState>,
    /// Description editor for the selected pinned snippet.
    description_input: Entity<InputState>,
    /// Echo guard: set while `sync_pin_inputs` writes programmatically so the
    /// resulting Change events never write back into the pins.
    syncing_pin_inputs: bool,
    /// Pinned section rows first (`pinned_count` of them), then Recent.
    visible: Vec<VisibleRow>,
    pinned_count: usize,
    /// How many filtered Recent rows to show; Load more raises it a page at
    /// a time so a huge log never renders all at once.
    display_limit: usize,
    next_cursor: Option<HistoryCursor>,
    /// Filtered Recent rows in total (before the display cap), so the list
    /// knows whether a Load more row is warranted.
    recent_filtered_total: usize,
    selected: Option<usize>,
    /// Test seam: when set, pins persist to this file instead of the default
    /// store. Only meaningful on native builds (wasm pins live in
    /// localStorage).
    #[allow(dead_code)]
    pins_file_override: Option<PathBuf>,
    /// Test seam: when set, the history store reads from this database path.
    #[allow(dead_code)]
    store_path_override: Option<PathBuf>,
    focus_handle: FocusHandle,
    list_scroll: ScrollHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl QueryHistoryBrowser {
    /// The allow drops out once the host's open action constructs the dialog.
    #[allow(dead_code)]
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.history_browser.filter_placeholder",
            ))
        });
        let filter_subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    if this.filter != text {
                        this.filter = text;
                        this.refresh_entries_from_store();
                        this.rebuild_visible();
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => this.open_selected(cx),
                _ => {}
            },
        );
        let tags_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.history_browser.tags_placeholder",
            ))
        });
        let tags_subscription = cx.subscribe_in(
            &tags_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    if this.syncing_pin_inputs {
                        return;
                    }
                    this.apply_tags(input.read(cx).value().to_string(), cx);
                }
            },
        );
        let name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.history_browser.name_placeholder",
            ))
        });
        let name_subscription = cx.subscribe_in(
            &name_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    if this.syncing_pin_inputs {
                        return;
                    }
                    this.apply_name(input.read(cx).value().to_string(), cx);
                }
            },
        );
        let description_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.history_browser.description_placeholder",
            ))
        });
        let description_subscription = cx.subscribe_in(
            &description_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    if this.syncing_pin_inputs {
                        return;
                    }
                    this.apply_description(input.read(cx).value().to_string(), cx);
                }
            },
        );
        let retention_removed = query_history::apply_retention().unwrap_or(0);
        let initial_page = query_history::read_page("", None, READ_LIMIT);
        let mut this = Self {
            entries: initial_page.entries,
            pins: query_history::load_pins(),
            retention_removed,
            filter: String::new(),
            filter_input,
            tags_input,
            name_input,
            description_input,
            syncing_pin_inputs: false,
            visible: Vec::new(),
            pinned_count: 0,
            display_limit: RECENT_DISPLAY_CAP,
            next_cursor: initial_page.next,
            recent_filtered_total: 0,
            selected: None,
            pins_file_override: None,
            store_path_override: None,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(600.0)),
            _subscriptions: vec![
                filter_subscription,
                tags_subscription,
                name_subscription,
                description_subscription,
            ],
        };
        this.rebuild_visible();
        this
    }

    /// Focus the search field — the host should call this right after opening
    /// so typing filters immediately (same contract as the project file
    /// picker's `focus_filter`). The allow drops out once the host wires it.
    #[allow(dead_code)]
    pub fn focus_filter(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input.update(cx, |state, cx| {
            state.focus(window, cx);
        });
    }

    // -- state ---------------------------------------------------------------

    /// Recompute the visible rows from the pins + entries under the current
    /// filter, keeping the selection on the same row when it survives.
    fn refresh_entries_from_store(&mut self) {
        let page = query_history::read_page(&self.filter, None, READ_LIMIT);
        self.entries = page.entries;
        self.next_cursor = page.next;
        self.recent_filtered_total = self.entries.len();
        self.display_limit = RECENT_DISPLAY_CAP;
    }

    fn rebuild_visible(&mut self) {
        let selected_key = self.selected.and_then(|index| {
            self.visible.get(index).map(|row| {
                (
                    row.entry.id,
                    row.entry.sql.clone(),
                    row.entry.server.clone(),
                    row.pinned,
                )
            })
        });

        let pinned: Vec<VisibleRow> = query_history::filter_entries(&self.pins, &self.filter)
            .into_iter()
            .map(|entry| VisibleRow {
                entry: entry.clone(),
                pinned: true,
            })
            .collect();
        self.recent_filtered_total = self.entries.len();
        let recent_filtered: Vec<HistoryEntry> =
            query_history::filter_entries(&self.entries, &self.filter)
                .into_iter()
                .cloned()
                .collect();
        self.recent_filtered_total = recent_filtered.len();
        let recent = recent_filtered
            .into_iter()
            .take(self.display_limit)
            .map(|entry| VisibleRow {
                entry,
                pinned: false,
            });
        self.pinned_count = pinned.len();
        self.visible = pinned.into_iter().chain(recent).collect();

        self.selected = selected_key
            .and_then(|(id, sql, server, was_pinned)| {
                self.visible.iter().position(|row| {
                    row.pinned == was_pinned
                        && ((id != 0 && row.entry.id == id)
                            || (id == 0 && row.entry.sql == sql && row.entry.server == server))
                })
            })
            .or_else(|| (!self.visible.is_empty()).then_some(0));
    }

    fn selected_row(&self) -> Option<&VisibleRow> {
        self.selected.and_then(|index| self.visible.get(index))
    }

    /// Load the tag/name/description editors with the selected pin's values
    /// (blank when the selection is not pinned). Guarded so the programmatic
    /// `set_value` echoes never write back into the pins.
    fn sync_pin_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pin = self
            .selected_row()
            .filter(|row| query_history::is_pinned(&self.pins, &row.entry))
            .and_then(|row| {
                self.pins
                    .iter()
                    .find(|pin| pin.sql == row.entry.sql && pin.server == row.entry.server)
            });
        let tags = pin.map(|pin| pin.tags.join(", ")).unwrap_or_default();
        let name = pin.map(|pin| pin.name.clone()).unwrap_or_default();
        let description = pin.map(|pin| pin.description.clone()).unwrap_or_default();
        self.syncing_pin_inputs = true;
        self.tags_input.update(cx, |state, cx| {
            state.set_value(tags, window, cx);
        });
        self.name_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.description_input.update(cx, |state, cx| {
            state.set_value(description, window, cx);
        });
        self.syncing_pin_inputs = false;
    }

    /// Persist an edit onto the selected pin via `set`, mirroring the change
    /// into the visible rows without a rebuild so typing never reorders the
    /// list mid-edit.
    fn apply_pin_edit(
        &mut self,
        cx: &mut Context<Self>,
        set: impl FnOnce(&mut [HistoryEntry], &HistoryEntry) -> bool,
    ) {
        let Some(entry) = self
            .selected_row()
            .map(|row| row.entry.clone())
            .filter(|entry| query_history::is_pinned(&self.pins, entry))
        else {
            return;
        };
        if set(&mut self.pins, &entry) {
            let updated = self
                .pins
                .iter()
                .find(|pin| pin.sql == entry.sql && pin.server == entry.server)
                .cloned();
            if let Some(updated) = updated {
                for row in &mut self.visible {
                    if row.entry.sql == entry.sql && row.entry.server == entry.server {
                        row.entry.tags = updated.tags.clone();
                        row.entry.name = updated.name.clone();
                        row.entry.description = updated.description.clone();
                    }
                }
            }
            self.persist_pins();
            cx.notify();
        }
    }

    fn apply_tags(&mut self, text: String, cx: &mut Context<Self>) {
        self.apply_pin_edit(cx, |pins, entry| {
            query_history::set_pin_tags(pins, entry, &text)
        });
    }

    fn apply_name(&mut self, text: String, cx: &mut Context<Self>) {
        self.apply_pin_edit(cx, |pins, entry| {
            query_history::set_pin_name(pins, entry, &text)
        });
    }

    fn apply_description(&mut self, text: String, cx: &mut Context<Self>) {
        self.apply_pin_edit(cx, |pins, entry| {
            query_history::set_pin_description(pins, entry, &text)
        });
    }

    /// Show another page of filtered Recent rows, re-reading a larger slice
    /// of the log when the current read is exhausted but the log may hold
    /// more.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        self.display_limit += RECENT_DISPLAY_CAP;
        if self.display_limit > self.entries.len() && self.next_cursor.is_some() {
            let cursor = self.next_cursor.clone().expect("cursor");
            let page = query_history::read_page(&self.filter, Some(&cursor), READ_LIMIT);
            self.entries.extend(page.entries);
            self.next_cursor = page.next;
        }
        self.recent_filtered_total = self.entries.len();
        self.rebuild_visible();
        cx.notify();
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.visible.len() && self.selected != Some(index) {
            self.selected = Some(index);
            self.sync_pin_inputs(window, cx);
            cx.notify();
        }
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        let next = match self.selected {
            Some(index) => index.saturating_add_signed(delta).min(last),
            None => 0,
        };
        if self.selected != Some(next) {
            self.selected = Some(next);
            self.list_scroll.scroll_to_item(self.child_index(next));
            self.sync_pin_inputs(window, cx);
            cx.notify();
        }
    }

    /// The scroll-container child index of visible row `row` (section headers
    /// occupy child slots of their own).
    fn child_index(&self, row: usize) -> usize {
        if row < self.pinned_count {
            1 + row
        } else {
            let pinned_block = if self.pinned_count > 0 {
                self.pinned_count + 1
            } else {
                0
            };
            pinned_block + 1 + (row - self.pinned_count)
        }
    }

    // -- actions -------------------------------------------------------------

    fn history_analysis_input(
        entry: &HistoryEntry,
    ) -> crate::ai_history_prompt::HistoryAnalysisInput<'_> {
        crate::ai_history_prompt::HistoryAnalysisInput {
            sql: &entry.sql,
            server: &entry.server,
            database: entry.database.as_deref(),
            timestamp: &entry.timestamp,
            duration_ms: entry.duration_ms,
            outcome: entry.outcome.as_log_value(),
            rows_affected: entry.affected_rows,
            error_text: None,
            plan_digest: None,
        }
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected_row() {
            Some(row) => {
                let sql = row.entry.sql.clone();
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Close);
            }
            None => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("query.history_browser.open_requires_selection"),
            }),
        }
    }

    /// One-click run: open the selected statement in a new tab and execute it
    /// immediately (no review pause — the entry is SQL the user already ran
    /// or pinned).
    fn run_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected_row() {
            Some(row) => {
                let sql = row.entry.sql.clone();
                cx.emit(ToolDialogEvent::ScriptToEditorAndRun { sql });
                cx.emit(ToolDialogEvent::Close);
            }
            None => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("query.history_browser.run_requires_selection"),
            }),
        }
    }

    fn analyze_selected(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text(
                    "query.history_browser.analyze_requires_selection",
                ),
            });
            return;
        };
        let input = Self::history_analysis_input(&row.entry);
        let prompt = crate::ai_history_prompt::build_history_analysis_prompt(&input);
        cx.emit(ToolDialogEvent::AnalyzeWithAi { prompt });
        cx.emit(ToolDialogEvent::Close);
    }

    fn analyze_recent_visible(&mut self, cx: &mut Context<Self>) {
        let recent: Vec<_> = self
            .visible
            .iter()
            .skip(self.pinned_count)
            .take(5)
            .map(|row| Self::history_analysis_input(&row.entry))
            .collect();
        if recent.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("query.history_browser.no_recent_to_analyze"),
            });
            return;
        }
        let prompt = crate::ai_history_prompt::build_history_range_prompt(&recent);
        cx.emit(ToolDialogEvent::AnalyzeWithAi { prompt });
        cx.emit(ToolDialogEvent::Close);
    }

    fn copy_rollback_sql_selected(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text(
                    "query.history_browser.rollback_requires_selection",
                ),
            });
            return;
        };
        let Some(rollback) = row.entry.rollback_sql.as_ref() else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("query.history_browser.no_rollback_sql"),
            });
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(rollback.clone()));
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Success,
            message: crate::localization::text("query.history_browser.rollback_copied"),
        });
    }

    fn toggle_selected_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_row().map(|row| row.entry.clone()) else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("query.history_browser.pin_requires_selection"),
            });
            return;
        };
        query_history::toggle_pin(&mut self.pins, &entry);
        self.persist_pins();
        self.rebuild_visible();
        // Follow the entry into whichever section it now lives in.
        self.selected = self
            .visible
            .iter()
            .position(|row| row.entry.sql == entry.sql && row.entry.server == entry.server)
            .or(self.selected);
        self.sync_pin_inputs(window, cx);
        cx.notify();
    }

    fn persist_pins(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = &self.pins_file_override {
            let _ = query_history::save_pins_to(path, &self.pins);
            return;
        }
        let _ = query_history::save_pins(&self.pins);
    }

    /// Export the pinned snippets to a JSON file the user picks. The file is
    /// the pins store's own format, so it also serves as a backup.
    #[cfg(not(target_arch = "wasm32"))]
    fn export_pins(&mut self, cx: &mut Context<Self>) {
        if self.pins.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("query.history_browser.no_pins_to_export"),
            });
            return;
        }
        let json = query_history::export_pins_json(&self.pins);
        let count = self.pins.len();
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("query-pins.json"));
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let result = std::fs::write(&path, json.as_bytes());
                let mut cx = async_cx.clone();
                weak.update(&mut cx, |_this, cx| {
                    cx.emit(ToolDialogEvent::Status {
                        level: match result {
                            Ok(()) => StatusLevel::Success,
                            Err(_) => StatusLevel::Error,
                        },
                        message: match result {
                            Ok(()) => crate::localization::format(
                                "query.history_browser.pins_exported",
                                &[
                                    ("count", &count.to_string()),
                                    ("path", &path.display().to_string()),
                                ],
                            ),
                            Err(error) => crate::localization::format(
                                "query.history_browser.pins_export_failed",
                                &[("error", &error.to_string())],
                            ),
                        },
                    });
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Write pinned snippets into the SQL Library `Pinned` folder as `.sql` files.
    #[cfg(not(target_arch = "wasm32"))]
    fn export_pins_to_library(&mut self, cx: &mut Context<Self>) {
        if self.pins.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text(
                    "query.history_browser.no_pins_for_library_export",
                ),
            });
            return;
        }
        let root = crate::preferences::SqlLibraryPreferences::load().effective_root();
        let files = pins_to_library_files(&self.pins, &root);
        let n = files.len();
        let dir = root.join("Pinned");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: crate::localization::format(
                    "query.history_browser.library_folder_failed",
                    &[("error", &error.to_string())],
                ),
            });
            return;
        }
        let mut written = 0usize;
        let mut last_error = None;
        for (path, contents) in files {
            match std::fs::write(&path, contents.as_bytes()) {
                Ok(()) => written += 1,
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        let message = match last_error {
            None => crate::localization::format(
                "query.history_browser.library_exported",
                &[("count", &written.to_string())],
            ),
            Some(error) => crate::localization::format(
                "query.history_browser.library_export_partial",
                &[
                    ("written", &written.to_string()),
                    ("total", &n.to_string()),
                    ("error", &error),
                ],
            ),
        };
        cx.emit(ToolDialogEvent::Status {
            level: if written == n {
                StatusLevel::Success
            } else {
                StatusLevel::Error
            },
            message,
        });
        cx.notify();
    }

    /// Merge pinned snippets from a previously exported JSON file, skipping
    /// ones already pinned.
    #[cfg(not(target_arch = "wasm32"))]
    fn import_pins(&mut self, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                crate::localization::text("query.history_browser.import_file_filter"),
                &["json"],
            )
            .pick_file()
        else {
            return;
        };
        let outcome = std::fs::read_to_string(&path)
            .map_err(|error| {
                crate::localization::format(
                    "query.history_browser.import_read_failed",
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &error.to_string()),
                    ],
                )
            })
            .and_then(|text| query_history::import_pins_json(&text));
        match outcome {
            Ok(imported) => {
                let added = query_history::merge_pins(&mut self.pins, imported);
                self.persist_pins();
                self.rebuild_visible();
                cx.emit(ToolDialogEvent::Status {
                    level: StatusLevel::Success,
                    message: crate::localization::format(
                        "query.history_browser.pins_imported",
                        &[("count", &added.to_string())],
                    ),
                });
                cx.notify();
            }
            Err(message) => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message,
            }),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    // Escape is handled by the dialog chrome (`on_close`); this keeps the
    // richer list navigation the chrome doesn't provide.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "up" => self.move_selection(-1, window, cx),
            "down" => self.move_selection(1, window, cx),
            "enter" => self.open_selected(cx),
            _ => {}
        }
    }

    // -- rendering -----------------------------------------------------------

    fn section_header(&self, label: String) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .px(px(s(12.0)))
            .pt(px(s(8.0)))
            .pb(px(s(3.0)))
            .text_size(px(Theme::font_size_ratio(0.77)))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Theme::text_dim())
            .child(label)
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let row = &self.visible[index];
        let is_selected = self.selected == Some(index);

        let preview = sql_preview_line(&row.entry.sql);
        // A named pin is a saved query: its row leads with the name and the
        // SQL preview moves into the meta line.
        let named = row.pinned && !row.entry.name.is_empty();
        let mut meta = if named {
            format!("{preview} · {}", row.entry.server)
        } else {
            row.entry.server.clone()
        };
        if let Some(database) = &row.entry.database {
            meta.push_str(&format!(" · {database}"));
        }
        meta.push_str(&format!(
            " · {}",
            timestamp_label(&row.entry.timestamp, chrono::Utc::now())
        ));
        match row.entry.outcome {
            query_history::HistoryOutcome::Succeeded => meta.push_str(&crate::localization::text(
                "query.history_browser.outcome_succeeded",
            )),
            query_history::HistoryOutcome::Failed => meta.push_str(&crate::localization::text(
                "query.history_browser.outcome_failed",
            )),
            query_history::HistoryOutcome::Cancelled => meta.push_str(&crate::localization::text(
                "query.history_browser.outcome_cancelled",
            )),
            query_history::HistoryOutcome::Unknown => {}
        }
        if let Some(duration_ms) = row.entry.duration_ms {
            meta.push_str(&format!(
                " · {}",
                crate::results::format_duration_ms(duration_ms)
            ));
        }
        if row.entry.run_count > 1 {
            meta.push_str(&format!(" · ×{}", row.entry.run_count));
        }
        if let Some(affected_rows) = row.entry.affected_rows {
            meta.push_str(&crate::localization::format(
                "query.history_browser.rows_affected",
                &[("count", &affected_rows.to_string())],
            ));
        }
        match row.entry.provenance {
            query_history::HistoryProvenance::Ai => meta.push_str(&crate::localization::text(
                "query.history_browser.provenance_ai",
            )),
            query_history::HistoryProvenance::Tool => meta.push_str(&crate::localization::text(
                "query.history_browser.provenance_tool",
            )),
            query_history::HistoryProvenance::Manual => {}
        }
        if !row.pinned && query_history::is_pinned(&self.pins, &row.entry) {
            meta.push_str(&crate::localization::text(
                "query.history_browser.pinned_badge",
            ));
        }
        if row.pinned && !row.entry.tags.is_empty() {
            meta.push_str(&format!(" · #{}", row.entry.tags.join(" #")));
        }
        if row.pinned && !row.entry.description.is_empty() {
            meta.push_str(&format!(" · {}", row.entry.description));
        }

        div()
            .id(("qh-row", index))
            .w_full()
            .px(px(s(12.0)))
            .py(px(s(5.0)))
            .cursor_pointer()
            .bg(if is_selected {
                Theme::selection()
            } else if index % 2 == 1 {
                Theme::row_alt()
            } else {
                Theme::panel_bg()
            })
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                this.select(index, window, cx);
                if ev.click_count() >= 2 {
                    this.open_selected(cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(1.0)))
                    .child(if named {
                        // Saved-query name: UI font + semibold so it reads as
                        // a title rather than SQL.
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(row.entry.name.clone())
                    } else {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .font_family(Theme::editor_font_family())
                            .text_color(Theme::text())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(preview)
                    })
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(meta),
                    ),
            )
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        if self.pinned_count > 0 {
            children.push(
                self.section_header(crate::localization::text(
                    "query.history_browser.pinned_section",
                ))
                .into_any_element(),
            );
            for index in 0..self.pinned_count {
                children.push(self.render_row(index, cx).into_any_element());
            }
        }
        if self.visible.len() > self.pinned_count {
            children.push(
                self.section_header(crate::localization::text(
                    "query.history_browser.recent_section",
                ))
                .into_any_element(),
            );
            for index in self.pinned_count..self.visible.len() {
                children.push(self.render_row(index, cx).into_any_element());
            }
        }
        let recent_shown = self.visible.len() - self.pinned_count;
        let more_filtered = self.recent_filtered_total > recent_shown;
        let log_may_hold_more = self.next_cursor.is_some();
        if recent_shown > 0 && (more_filtered || log_may_hold_more) {
            let remaining = self
                .recent_filtered_total
                .saturating_sub(recent_shown)
                .max(if log_may_hold_more { 1 } else { 0 });
            let label = if more_filtered || log_may_hold_more {
                crate::localization::format(
                    "query.history_browser.load_more",
                    &[
                        ("remaining", &remaining.to_string()),
                        (
                            "more_suffix",
                            &if self.next_cursor.is_some() {
                                "+".to_string()
                            } else {
                                String::new()
                            },
                        ),
                    ],
                )
            } else {
                crate::localization::text("query.history_browser.load_more_from_log")
            };
            children.push(
                div()
                    .id("qh-load-more")
                    .px(px(s(12.0)))
                    .py(px(s(6.0)))
                    .cursor_pointer()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::accent())
                    .hover(|style| style.bg(Theme::row_hover()))
                    .child(label)
                    .on_click(cx.listener(|this, _, _window, cx| this.load_more(cx)))
                    .into_any_element(),
            );
        }
        if self.visible.is_empty() {
            let message = if self.entries.is_empty() && self.pins.is_empty() {
                crate::localization::text("query.history_browser.empty_history")
            } else {
                crate::localization::text("query.history_browser.empty_search")
            };
            children.push(
                div()
                    .p(px(s(16.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text_dim())
                    .child(message)
                    .into_any_element(),
            );
        }

        div()
            .id("qh-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .children(children)
    }
}

impl gpui::Focusable for QueryHistoryBrowser {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueryHistoryBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let has_selection = self.selected_row().is_some();
        let has_rollback = self
            .selected_row()
            .is_some_and(|row| row.entry.rollback_sql.is_some());
        let selection_pinned = self
            .selected_row()
            .is_some_and(|row| query_history::is_pinned(&self.pins, &row.entry));
        let recent_shown = self.visible.len() - self.pinned_count;

        let focus_handle = self.focus_handle.clone();
        let content = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(14.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(div().flex_1())
                    .child(
                        Button::new("qh-open")
                            .small()
                            .label(crate::localization::text(
                                "query.history_browser.open_button",
                            ))
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, _window, cx| this.open_selected(cx))),
                    )
                    .child(
                        Button::new("qh-run")
                            .small()
                            .label(crate::localization::text(
                                "query.history_browser.run_button",
                            ))
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, _window, cx| this.run_selected(cx))),
                    )
                    .child(
                        Button::new("qh-analyze")
                            .small()
                            .label(crate::localization::text(
                                "query.history_browser.analyze_button",
                            ))
                            .disabled(!has_selection)
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.analyze_selected(cx)),
                            ),
                    )
                    .child(
                        Button::new("qh-analyze-recent")
                            .small()
                            .label(crate::localization::text(
                                "query.history_browser.analyze_recent_button",
                            ))
                            .disabled(recent_shown == 0)
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.analyze_recent_visible(cx)),
                            ),
                    )
                    .child(
                        Button::new("qh-copy-rollback")
                            .small()
                            .label(crate::localization::text(
                                "query.history_browser.copy_rollback_button",
                            ))
                            .disabled(!has_rollback)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.copy_rollback_sql_selected(cx)
                            })),
                    )
                    .child(
                        Button::new("qh-pin")
                            .small()
                            .label(if selection_pinned {
                                crate::localization::text("query.history_browser.unpin_button")
                            } else {
                                crate::localization::text("query.history_browser.pin_button")
                            })
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_selected_pin(window, cx)
                            })),
                    )
                    .children({
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            vec![
                                Button::new("qh-export-pins")
                                    .small()
                                    .label(crate::localization::text(
                                        "query.history_browser.export_pins_button",
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, _window, cx| this.export_pins(cx)),
                                    ),
                                Button::new("qh-export-library")
                                    .small()
                                    .label(crate::localization::text(
                                        "query.history_browser.export_library_button",
                                    ))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.export_pins_to_library(cx)
                                    })),
                                Button::new("qh-import-pins")
                                    .small()
                                    .label(crate::localization::text(
                                        "query.history_browser.import_pins_button",
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, _window, cx| this.import_pins(cx)),
                                    ),
                            ]
                        }
                        #[cfg(target_arch = "wasm32")]
                        {
                            Vec::<Button>::new()
                        }
                    }),
            )
            .child(Input::new(&self.filter_input).cleanable(true))
            .when(selection_pinned, |dialog| {
                let label = |text: String| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.84)))
                        .text_color(Theme::text_dim())
                        .child(text)
                };
                dialog
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(label(crate::localization::text(
                                "query.history_browser.name_label",
                            )))
                            .child(div().flex_1().child(Input::new(&self.name_input)))
                            .child(label(crate::localization::text(
                                "query.history_browser.description_label",
                            )))
                            .child(div().flex_1().child(Input::new(&self.description_input))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(label(crate::localization::text(
                                "query.history_browser.tags_label",
                            )))
                            .child(div().flex_1().child(Input::new(&self.tags_input))),
                    )
            })
            .child(self.render_list(cx))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "query.history_browser.footer",
                        &[
                            ("pinned_count", &self.pinned_count.to_string()),
                            ("recent_count", &recent_shown.to_string()),
                            ("removed_count", &self.retention_removed.to_string()),
                        ],
                    )),
            );

        Dialog::new("query-history-browser", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "query.history_browser.dialog_title",
            ))
            .easy_close(true)
            .key_context("QueryHistoryBrowser")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .bare_body()
            .child(content)
            .build(self, window, cx)
            // Key events bubble from the focused card to this overlay, so
            // arrow/Enter list navigation still fires alongside the chrome's
            // Escape handling.
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sql: &str, server: &str, database: Option<&str>, timestamp: &str) -> HistoryEntry {
        entry_with_duration(sql, server, database, timestamp, None)
    }

    fn entry_with_duration(
        sql: &str,
        server: &str,
        database: Option<&str>,
        timestamp: &str,
        duration_ms: Option<u64>,
    ) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            sql: sql.to_string(),
            server: server.to_string(),
            database: database.map(str::to_string),
            timestamp: timestamp.to_string(),
            duration_ms,
            outcome: query_history::HistoryOutcome::Unknown,
            tags: Vec::new(),
            name: String::new(),
            description: String::new(),
            run_count: 1,
            provenance: query_history::HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn sql_preview_uses_the_first_nonempty_line() {
        assert_eq!(sql_preview_line("SELECT 1\nFROM t"), "SELECT 1");
        assert_eq!(
            sql_preview_line("\n\n   UPDATE x SET y = 1  \nWHERE z"),
            "UPDATE x SET y = 1"
        );
        assert_eq!(sql_preview_line(""), "");
    }

    #[test]
    fn sql_preview_truncates_long_lines_with_an_ellipsis() {
        let long = "S".repeat(300);
        let preview = sql_preview_line(&long);
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
        // At the boundary nothing is appended.
        let exact = "x".repeat(PREVIEW_MAX_CHARS);
        assert_eq!(sql_preview_line(&exact), exact);
    }

    #[test]
    fn timestamp_label_buckets_relative_ages() {
        let now =
            chrono::NaiveDateTime::parse_from_str("2026-08-24T12:00:00Z", "%Y-%m-%dT%H:%M:%SZ")
                .unwrap()
                .and_utc();
        let label = |ts: &str| timestamp_label(ts, now);
        assert_eq!(label("2026-08-24T11:59:30Z"), "just now");
        assert_eq!(label("2026-08-24T11:55:00Z"), "5m ago");
        assert_eq!(label("2026-08-24T09:00:00Z"), "3h ago");
        assert_eq!(label("2026-08-22T12:00:00Z"), "2d ago");
        assert_eq!(label("2026-08-01T00:00:00Z"), "2026-08-01");
        // Future timestamps (clock skew) fall back to the date.
        assert_eq!(label("2026-08-25T12:00:00Z"), "2026-08-25");
    }

    #[test]
    fn timestamp_label_passes_garbage_through() {
        let now = chrono::Utc::now();
        assert_eq!(timestamp_label("not-a-time", now), "not-a-time");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn pins_to_library_files_disambiguates_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = entry("SELECT 1", "Prod", Some("sales"), "2026-08-30T00:00:00Z");
        a.name = "orders".into();
        let mut b = entry("SELECT 2", "Prod", Some("sales"), "2026-08-30T00:00:01Z");
        b.name = "orders".into();
        let files = pins_to_library_files(&[a, b], dir.path());
        assert_eq!(files.len(), 2);
        assert_ne!(files[0].0, files[1].0);
        let pinned = dir.path().join("Pinned");
        assert!(files[0].0.starts_with(&pinned));
        assert!(files[1].0.starts_with(&pinned));
        assert!(files[0].1.starts_with("-- sqlly:"));
        assert!(files[1].1.starts_with("-- sqlly:"));
    }

    // -- windowed view tests -------------------------------------------------

    /// gpui-component widgets read the library's global theme during render;
    /// windowed tests must install it (idempotently) before opening windows.
    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn make_browser(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::Entity<QueryHistoryBrowser>,
        &mut gpui::VisualTestContext,
    ) {
        ensure_components_initialized(cx);
        cx.add_window_view(QueryHistoryBrowser::new)
    }

    /// Swap in deterministic data (construction reads whatever log the
    /// machine has).
    fn seed(
        browser: &mut QueryHistoryBrowser,
        entries: Vec<HistoryEntry>,
        pins: Vec<HistoryEntry>,
    ) {
        browser.entries = entries;
        browser.pins = pins;
        browser.filter.clear();
        browser.selected = None;
        browser.rebuild_visible();
    }

    const TS: &str = "2026-08-24T10:00:00Z";

    #[gpui::test]
    fn filtering_narrows_rows_and_keeps_a_valid_selection(cx: &mut gpui::TestAppContext) {
        let (view, cx) = make_browser(cx);
        view.update_in(cx, |this, window, cx| {
            seed(
                this,
                vec![
                    entry("SELECT * FROM Orders", "prod", Some("Sales"), TS),
                    entry("UPDATE users SET active = 1", "dev", None, TS),
                ],
                vec![entry("EXEC sp_who2", "prod", None, TS)],
            );
            assert_eq!(this.pinned_count, 1);
            assert_eq!(this.visible.len(), 3);
            assert_eq!(this.selected, Some(0), "defaults to the first row");

            // Select the recent UPDATE row, then filter it away: selection
            // falls back to the first surviving row.
            this.select(2, window, cx);
            this.filter = "orders".to_string();
            this.rebuild_visible();
            assert_eq!(this.pinned_count, 0, "pin doesn't match the needle");
            assert_eq!(this.visible.len(), 1);
            assert_eq!(this.selected, Some(0));
            assert_eq!(this.visible[0].entry.server, "prod");

            // Server matching keeps the pinned section too.
            this.filter = "prod".to_string();
            this.rebuild_visible();
            assert_eq!(this.pinned_count, 1);
            assert_eq!(this.visible.len(), 2);
        });
    }

    #[gpui::test]
    fn toggling_a_pin_persists_and_moves_the_entry_between_sections(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let pins_file = dir.path().join("pins.json");
        let (view, cx) = make_browser(cx);
        view.update_in(cx, |this, window, cx| {
            this.pins_file_override = Some(pins_file.clone());
            seed(
                this,
                vec![entry("SELECT 1", "srv", Some("db"), TS)],
                Vec::new(),
            );
            assert_eq!(this.pinned_count, 0);

            this.toggle_selected_pin(window, cx);
            assert_eq!(this.pinned_count, 1);
            assert_eq!(this.visible.len(), 2, "pinned + still recent");
            assert_eq!(this.selected, Some(0), "selection follows into Pinned");
        });
        let stored = crate::query_history::load_pins_from(&pins_file);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].sql, "SELECT 1");

        view.update_in(cx, |this, window, cx| {
            this.toggle_selected_pin(window, cx);
            assert_eq!(this.pinned_count, 0);
            assert_eq!(this.visible.len(), 1);
        });
        assert!(crate::query_history::load_pins_from(&pins_file).is_empty());
    }

    #[gpui::test]
    fn open_selected_scripts_the_full_sql_and_closes(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (view, cx) = make_browser(cx);
        view.update(cx, |this, _cx| {
            seed(
                this,
                vec![entry("SELECT 42\nFROM t\nWHERE x = 1", "srv", None, TS)],
                Vec::new(),
            );
        });

        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = seen.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ToolDialogEvent, _cx| {
                sink.borrow_mut().push(match event {
                    ToolDialogEvent::ScriptToEditor { sql } => format!("script:{sql}"),
                    ToolDialogEvent::ScriptToEditorForProfile { sql, .. } => {
                        format!("script:{sql}")
                    }
                    ToolDialogEvent::ScriptToEditorAndRun { sql } => format!("run:{sql}"),
                    ToolDialogEvent::ScriptToEditorForProfileAndRun { sql, .. } => {
                        format!("run:{sql}")
                    }
                    ToolDialogEvent::Close => "close".to_string(),
                    ToolDialogEvent::Status { .. } => "status".to_string(),
                    ToolDialogEvent::DumpCompleted { .. } => "dump_completed".to_string(),
                    ToolDialogEvent::DumpStarted { .. } => "dump_started".to_string(),
                    ToolDialogEvent::TransferStarted => "transfer_started".to_string(),
                    ToolDialogEvent::TransferFinished { .. } => "transfer_finished".to_string(),
                    ToolDialogEvent::SaveDumpSchedule { .. } => "save_dump_schedule".to_string(),
                    ToolDialogEvent::AnalyzeWithAi { .. } => "analyze".to_string(),
                    ToolDialogEvent::OpenTool(_) => "open_tool".to_string(),
                    ToolDialogEvent::SaveSchemaSnapshot { .. } => {
                        "save_schema_snapshot".to_string()
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    ToolDialogEvent::RunSqlFile { .. } => "run_sql_file".to_string(),
                });
            })
            .detach();
        });

        view.update(cx, |this, cx| this.open_selected(cx));
        cx.run_until_parked();
        assert_eq!(
            seen.borrow().as_slice(),
            ["script:SELECT 42\nFROM t\nWHERE x = 1", "close"]
        );

        // With nothing selected only a status nudge is emitted.
        seen.borrow_mut().clear();
        view.update(cx, |this, cx| {
            seed(this, Vec::new(), Vec::new());
            assert_eq!(this.selected, None);
            this.open_selected(cx);
        });
        cx.run_until_parked();
        assert_eq!(seen.borrow().as_slice(), ["status"]);
    }

    #[gpui::test]
    fn run_selected_emits_script_and_run_then_closes(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (view, cx) = make_browser(cx);
        view.update(cx, |this, _cx| {
            seed(
                this,
                vec![entry("SELECT 42\nFROM t", "srv", None, TS)],
                Vec::new(),
            );
        });

        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = seen.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ToolDialogEvent, _cx| {
                sink.borrow_mut().push(match event {
                    ToolDialogEvent::ScriptToEditorAndRun { sql } => format!("run:{sql}"),
                    ToolDialogEvent::ScriptToEditorForProfileAndRun { sql, .. } => {
                        format!("run:{sql}")
                    }
                    ToolDialogEvent::Close => "close".to_string(),
                    _ => "other".to_string(),
                });
            })
            .detach();
        });

        view.update(cx, |this, cx| this.run_selected(cx));
        cx.run_until_parked();
        assert_eq!(seen.borrow().as_slice(), ["run:SELECT 42\nFROM t", "close"]);

        // With nothing selected only a status nudge is emitted.
        seen.borrow_mut().clear();
        view.update(cx, |this, cx| {
            seed(this, Vec::new(), Vec::new());
            this.run_selected(cx);
        });
        cx.run_until_parked();
        assert_eq!(seen.borrow().as_slice(), ["other"]);
    }

    #[gpui::test]
    fn analyze_selected_emits_prompt_then_closes(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (view, cx) = make_browser(cx);
        view.update(cx, |this, _cx| {
            seed(
                this,
                vec![entry_with_duration(
                    "SELECT 42\nFROM t",
                    "srv",
                    None,
                    TS,
                    Some(120),
                )],
                Vec::new(),
            );
        });

        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = seen.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ToolDialogEvent, _cx| {
                sink.borrow_mut().push(match event {
                    ToolDialogEvent::AnalyzeWithAi { prompt } => {
                        if prompt.contains("Elapsed: 120 ms") && !prompt.contains("secret") {
                            "analyze".to_string()
                        } else {
                            "analyze-bad".to_string()
                        }
                    }
                    ToolDialogEvent::Close => "close".to_string(),
                    _ => "other".to_string(),
                });
            })
            .detach();
        });

        view.update(cx, |this, cx| this.analyze_selected(cx));
        cx.run_until_parked();
        assert_eq!(seen.borrow().as_slice(), ["analyze", "close"]);
    }

    #[gpui::test]
    fn analyze_selected_without_selection_reports_status(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (view, cx) = make_browser(cx);
        view.update(cx, |this, _cx| seed(this, Vec::new(), Vec::new()));

        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = seen.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ToolDialogEvent, _cx| {
                sink.borrow_mut().push(match event {
                    ToolDialogEvent::Status { .. } => "status".to_string(),
                    _ => "other".to_string(),
                });
            })
            .detach();
        });

        view.update(cx, |this, cx| this.analyze_selected(cx));
        cx.run_until_parked();
        assert_eq!(seen.borrow().as_slice(), ["status"]);
    }

    #[gpui::test]
    fn naming_a_pin_persists_mirrors_rows_and_searches(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let pins_file = dir.path().join("pins.json");
        let (view, cx) = make_browser(cx);
        view.update(cx, |this, cx| {
            this.pins_file_override = Some(pins_file.clone());
            seed(
                this,
                vec![entry("SELECT 1", "srv", Some("db"), TS)],
                vec![entry("SELECT 1", "srv", Some("db"), TS)],
            );
            assert_eq!(this.selected, Some(0), "pinned row selected");

            this.apply_name("Daily check".to_string(), cx);
            this.apply_description("Smoke query for mornings".to_string(), cx);

            assert_eq!(this.pins[0].name, "Daily check");
            assert_eq!(this.pins[0].description, "Smoke query for mornings");
            // Both the pinned row and its Recent twin mirror the edit.
            for row in &this.visible {
                assert_eq!(row.entry.name, "Daily check");
                assert_eq!(row.entry.description, "Smoke query for mornings");
            }

            // Search matches the name and the description.
            this.filter = "daily".to_string();
            this.rebuild_visible();
            assert_eq!(this.pinned_count, 1);
            this.filter = "mornings".to_string();
            this.rebuild_visible();
            assert_eq!(this.pinned_count, 1);
            this.filter = "unrelated".to_string();
            this.rebuild_visible();
            assert_eq!(this.visible.len(), 0);
        });

        let stored = crate::query_history::load_pins_from(&pins_file);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "Daily check");
        assert_eq!(stored[0].description, "Smoke query for mornings");
    }

    #[gpui::test]
    fn load_more_appends_next_page(cx: &mut gpui::TestAppContext) {
        // Serialize with the other tests that drive the global store-path
        // override so their databases can't bleed into this paging read.
        #[cfg(not(target_arch = "wasm32"))]
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("h.db");
        #[cfg(not(target_arch = "wasm32"))]
        {
            crate::query_history_store::set_store_path_override(Some(db_path.clone()));
            let history = query_history::QueryHistory::with_store_path(db_path);
            for index in 0..250 {
                history.insert_at(
                    &query_history::HistoryRecord {
                        sql: &format!("SELECT {index}"),
                        server: "srv",
                        database: Some("db"),
                        duration_ms: None,
                        outcome: query_history::HistoryOutcome::Unknown,
                        provenance: query_history::HistoryProvenance::Manual,
                        affected_rows: None,
                        rollback_sql: None,
                    },
                    &format!("2026-08-01T10:{index:02}Z"),
                );
            }
        }
        let (view, cx) = make_browser(cx);
        view.update(cx, |this, _cx| {
            this.refresh_entries_from_store();
            this.rebuild_visible();
            let recent = this.visible.len() - this.pinned_count;
            assert_eq!(recent, RECENT_DISPLAY_CAP);
        });
        view.update(cx, |this, cx| {
            this.load_more(cx);
            let recent = this.visible.len() - this.pinned_count;
            assert_eq!(recent, 250);
            let ids: Vec<i64> = this
                .visible
                .iter()
                .skip(this.pinned_count)
                .map(|row| row.entry.id)
                .collect();
            let mut unique = ids.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), ids.len());
        });
        #[cfg(not(target_arch = "wasm32"))]
        crate::query_history_store::set_store_path_override(None);
    }
}
