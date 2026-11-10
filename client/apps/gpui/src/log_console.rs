use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Styled,
    Subscription, Window, div, point, prelude::*, px,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use parking_lot::Mutex;

use crate::icon_catalog::AppIcon;
use crate::theme::Theme;

/// Count shown on the status-bar Diagnostics icon when the log is closed.
pub fn hidden_badge_count(visible: bool, entries: usize) -> Option<usize> {
    if !visible && entries > 0 {
        Some(entries)
    } else {
        None
    }
}

/// Maximum bytes the active diagnostics log may reach before it is rotated to a
/// single `*.previous.log` sibling. Mirrors the Swift `maxBytes` (5 MB).
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Directory that holds the diagnostics log files:
/// `<data_dir>/SQLLY/Logs`. Mirrors the Swift `<applicationSupport>/SQLLY/Logs`.
pub fn diagnostic_log_dir() -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    // Browser build: nominal path only — there is no filesystem, so the
    // diagnostics *file* log is inert (writes below are all best-effort and
    // fail silently); the in-app log console works normally.
    #[cfg(target_arch = "wasm32")]
    let mut p = PathBuf::new();
    p.push("SQLLY");
    p.push("Logs");
    p
}

/// Active diagnostics log file path: `<log dir>/sqlly-diagnostics.log`.
pub fn diagnostic_log_file() -> PathBuf {
    let mut p = diagnostic_log_dir();
    p.push("sqlly-diagnostics.log");
    p
}

/// Rotated diagnostics log file path: `<log dir>/sqlly-diagnostics.previous.log`.
pub fn diagnostic_log_previous_file() -> PathBuf {
    let mut p = diagnostic_log_dir();
    p.push("sqlly-diagnostics.previous.log");
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLogLevel {
    Off,
    Error,
    Verbose,
}

impl DiagnosticLogLevel {
    #[allow(dead_code)]
    pub fn from_prefs(verbose: bool, error_only: bool) -> Self {
        if verbose {
            DiagnosticLogLevel::Verbose
        } else if error_only {
            DiagnosticLogLevel::Error
        } else {
            DiagnosticLogLevel::Off
        }
    }

    pub fn allows(&self, level: DiagnosticEntryLevel) -> bool {
        match self {
            DiagnosticLogLevel::Off => false,
            DiagnosticLogLevel::Error => level == DiagnosticEntryLevel::Error,
            DiagnosticLogLevel::Verbose => true,
        }
    }
}

impl From<crate::preferences::DiagnosticLogLevelPref> for DiagnosticLogLevel {
    fn from(pref: crate::preferences::DiagnosticLogLevelPref) -> Self {
        match pref {
            crate::preferences::DiagnosticLogLevelPref::Off => DiagnosticLogLevel::Off,
            crate::preferences::DiagnosticLogLevelPref::Error => DiagnosticLogLevel::Error,
            crate::preferences::DiagnosticLogLevelPref::Verbose => DiagnosticLogLevel::Verbose,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticEntryLevel {
    Verbose,
    Error,
}

#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    pub timestamp_ms: u128,
    pub level: DiagnosticEntryLevel,
    pub category: String,
    pub message: String,
}

impl DiagnosticEntry {
    #[allow(dead_code)]
    pub fn format_line(&self) -> String {
        let level_str = match self.level {
            DiagnosticEntryLevel::Verbose => "VRB",
            DiagnosticEntryLevel::Error => "ERR",
        };
        format!(
            "{} {} [{}] {}",
            self.timestamp_ms, level_str, self.category, self.message
        )
    }
}

#[derive(Clone)]
pub struct DiagnosticLog {
    entries: Arc<Mutex<Vec<DiagnosticEntry>>>,
    level: DiagnosticLogLevel,
    start: Instant,
    /// Optional on-disk sink. `None` disables file writing (used in tests that
    /// only exercise in-memory level gating). The macOS app seeds this with the
    /// real App Support path so Reveal/Open work.
    #[allow(dead_code)]
    file_path: Option<PathBuf>,
    /// Serializes writes (and rotation) to the log file across threads.
    file_lock: Arc<Mutex<()>>,
}

impl DiagnosticLog {
    #[allow(dead_code)]
    /// Creates an in-memory-only log (no file sink). Primarily for tests.
    pub fn new(level: DiagnosticLogLevel) -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
            level,
            start: Instant::now(),
            file_path: None,
            file_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Creates a log that also persists every allowed entry to `file_path`.
    pub fn with_file(level: DiagnosticLogLevel, file_path: PathBuf) -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
            level,
            start: Instant::now(),
            file_path: Some(file_path),
            file_lock: Arc::new(Mutex::new(())),
        }
    }

    /// The on-disk log file path, if file-backed.
    #[allow(dead_code)]
    pub fn file_path(&self) -> Option<PathBuf> {
        self.file_path.clone()
    }

    #[allow(dead_code)]
    pub fn level(&self) -> DiagnosticLogLevel {
        self.level
    }

    #[allow(dead_code)]
    pub fn set_level(&mut self, level: DiagnosticLogLevel) {
        self.level = level;
    }

    pub fn log(&self, level: DiagnosticEntryLevel, category: &str, message: &str) {
        if !self.level.allows(level) {
            return;
        }
        let entry = DiagnosticEntry {
            timestamp_ms: self.start.elapsed().as_millis(),
            level,
            category: category.to_string(),
            message: message.to_string(),
        };
        self.write_to_file(&entry);
        self.entries.lock().push(entry);
    }

    /// Appends a single formatted line to the log file, rotating first if the
    /// file has grown past `LOG_MAX_BYTES`. Failures are swallowed: logging must
    /// never crash or stall the app. Newlines in the payload are escaped so each
    /// record stays on a single grep-able line.
    fn write_to_file(&self, entry: &DiagnosticEntry) {
        let Some(path) = &self.file_path else {
            return;
        };
        let _guard = self.file_lock.lock();
        if let Some(dir) = path.parent()
            && !dir.exists()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        // Rotate if needed.
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() >= LOG_MAX_BYTES
        {
            let _ = std::fs::rename(path, diagnostic_log_previous_file());
        }
        let line = format!(
            "{}  {}  [{}] {}\n",
            wall_clock_timestamp(),
            match entry.level {
                DiagnosticEntryLevel::Verbose => "VRB",
                DiagnosticEntryLevel::Error => "ERR",
            },
            entry.category,
            entry.message.replace('\n', "\\n"),
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn log_error(&self, category: &str, message: &str) {
        self.log(DiagnosticEntryLevel::Error, category, message);
    }

    pub fn log_verbose(&self, category: &str, message: &str) {
        self.log(DiagnosticEntryLevel::Verbose, category, message);
    }

    pub fn entries(&self) -> Vec<DiagnosticEntry> {
        self.entries.lock().clone()
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    pub fn entry_count(&self) -> usize {
        self.entries.lock().len()
    }
}

/// Formats the current wall-clock time as `yyyy-MM-dd HH:mm:ss.SSS` (local).
/// Mirrors the Swift line-prefix format.
fn wall_clock_timestamp() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(target_arch = "wasm32")]
    use web_time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis();
    let secs = (millis / 1000) as i64;
    let ms = (millis % 1000) as u32;
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        year, m, d, hour, minute, second, ms
    )
}

fn format_entry_line(e: &DiagnosticEntry) -> String {
    let prefix = match e.level {
        DiagnosticEntryLevel::Error => "ERR",
        DiagnosticEntryLevel::Verbose => "VRB",
    };
    format!(
        "{} {} [{}] {}",
        e.timestamp_ms, prefix, e.category, e.message
    )
}

pub struct LogConsoleView {
    log: DiagnosticLog,
    /// The console body: one read-only, multi-line text field holding every
    /// entry.
    ///
    /// It used to be a stack of per-entry `div`s with line-granularity
    /// selection of its own — you could take whole lines and nothing else, so
    /// lifting one server name or one error message out of the log meant
    /// copying the line and trimming it somewhere else. A real text field
    /// selects by character, spans lines, knows ⌘A / ⌘C / find, and costs only
    /// the per-level coloring (the `ERR` / `VRB` marker on each line still says
    /// what the color used to).
    text_input: Entity<TextareaState>,
    /// What the field is supposed to be showing. Anything else in it is a user
    /// keystroke, and gets reverted — this is a log, not a scratch buffer.
    rendered: SharedString,
    visible: bool,
    auto_scroll: bool,
    last_entry_count: usize,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
pub enum LogConsoleEvent {
    #[allow(dead_code)]
    VisibilityChanged(bool),
}

impl EventEmitter<LogConsoleEvent> for LogConsoleView {}

impl LogConsoleView {
    pub fn new(log: DiagnosticLog, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                // Log lines are long and structured; wrapping them turns one
                // entry into an unpredictable number of rows and makes the
                // whole column hard to scan. Scroll sideways instead.
                .soft_wrap(false)
                .searchable(true)
        });

        // Read-only is declared on the rendered `Textarea` (`.readonly(true)`):
        // unlike `disabled`, it keeps full contrast and selection, ⌘C, and
        // find-all working while rejecting user edits, so the old
        // revert-on-change subscription is no longer needed.
        let subscriptions = Vec::new();

        Self {
            log,
            text_input,
            rendered: SharedString::default(),
            visible: false,
            auto_scroll: true,
            last_entry_count: 0,
            _subscriptions: subscriptions,
        }
    }

    pub fn toggle_visibility(&mut self, cx: &mut Context<Self>) {
        self.visible = !self.visible;
        cx.emit(LogConsoleEvent::VisibilityChanged(self.visible));
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            cx.emit(LogConsoleEvent::VisibilityChanged(visible));
            cx.notify();
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn entry_count(&self) -> usize {
        self.log.entry_count()
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.log.clear();
        self.last_entry_count = 0;
        cx.notify();
    }

    pub fn toggle_auto_scroll(&mut self, cx: &mut Context<Self>) {
        self.auto_scroll = !self.auto_scroll;
        cx.notify();
    }

    /// Push the log into the text field when it has moved on, preserving the
    /// user's selection.
    ///
    /// Byte offsets into the field survive an append — the log only ever grows
    /// at the end (or is cleared outright) — so a selection made a moment ago
    /// still covers the same characters after the next entry arrives.
    fn sync_text(
        &mut self,
        entries: &[DiagnosticEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The entry count only moves when the log does — except on Clear, which
        // takes it back to zero while the field still holds the old text.
        let count = entries.len();
        if count == self.last_entry_count && (count > 0 || self.rendered.is_empty()) {
            return;
        }
        let text: SharedString = entries
            .iter()
            .map(format_entry_line)
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        self.last_entry_count = count;
        if text == self.rendered {
            return;
        }
        self.rendered = text.clone();

        // `set_value` clears the selection and rewinds to the top. Neither is
        // wanted here: the text only grew at the end, so both the selection and
        // (when auto-scroll is off) the reading position are still meaningful.
        let (selection, offset) = {
            let state = self.text_input.read(cx);
            (state.selected_range(), state.scroll_offset())
        };
        let len = text.len();
        self.text_input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            if !selection.is_empty() && selection.end <= len {
                state.set_selected_range(selection, cx);
            }
        });
        if self.auto_scroll {
            self.scroll_to_end(cx);
        } else {
            self.text_input
                .update(cx, |state, cx| state.set_scroll_offset(offset, cx));
        }
    }

    /// Park the viewport on the last line. Done by hand rather than by moving
    /// the cursor: `set_cursor_position` focuses the field, and a log entry
    /// arriving is no reason to take the caret out of the editor the user is
    /// typing in.
    fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        let (line_height, visible_rows) = {
            let state = self.text_input.read(cx);
            (state.line_height(), state.visible_row_range())
        };
        let (Some(line_height), Some(visible)) = (line_height, visible_rows) else {
            return;
        };
        let total = self.rendered.lines().count();
        let overflow = total.saturating_sub(visible.len()) as f32;
        self.text_input.update(cx, |state, cx| {
            state.set_scroll_offset(point(px(0.0), px(-overflow * f32::from(line_height))), cx);
        });
    }

    /// The user's selection, or the whole log when nothing is selected.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let selected = self.text_input.read(cx).selected_value();
        let text = if selected.is_empty() {
            self.rendered.to_string()
        } else {
            selected.to_string()
        };
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
}

impl Render for LogConsoleView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }

        let entries = self.log.entries();
        let error_count = entries
            .iter()
            .filter(|e| e.level == DiagnosticEntryLevel::Error)
            .count();
        let entry_count = entries.len();

        self.sync_text(&entries, window, cx);

        let auto_scroll = self.auto_scroll;

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(Theme::sidebar_bg())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(Theme::bar_height(2.0)))
                    .items_center()
                    .px(px(Theme::scaled(8.0)))
                    .gap(px(Theme::scaled(8.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "tools.log_console.entry_count",
                                &[("count", &entry_count.to_string())],
                            )),
                    )
                    .child(if error_count > 0 {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::error())
                            .child(crate::localization::format(
                                "tools.log_console.error_count",
                                &[("count", &error_count.to_string())],
                            ))
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    .child(div().flex_1())
                    .child({
                        let this = cx.entity();
                        Button::new("log-overflow")
                            .icon(Icon::empty().path(AppIcon::MoreVertical.path()))
                            .ghost()
                            .xsmall()
                            .dropdown_menu(move |menu, _, _| {
                                let auto = this.clone();
                                let copy = this.clone();
                                let clear = this.clone();
                                menu.item(
                                    PopupMenuItem::new(crate::localization::text(
                                        "tools.log_console.auto_scroll_menu",
                                    ))
                                    .checked(auto_scroll)
                                    .on_click(
                                        move |_ev, _window, app| {
                                            auto.update(app, |this, cx| {
                                                this.toggle_auto_scroll(cx);
                                            });
                                        },
                                    ),
                                )
                                .item(
                                    PopupMenuItem::new(crate::localization::text(
                                        "tools.log_console.copy_menu",
                                    ))
                                    .on_click(
                                        move |_ev, _window, app| {
                                            copy.update(app, |this, cx| this.copy(cx));
                                        },
                                    ),
                                )
                                .item(
                                    PopupMenuItem::new(crate::localization::text(
                                        "tools.log_console.clear_menu",
                                    ))
                                    .on_click(
                                        move |_ev, _window, app| {
                                            clear.update(app, |this, cx| this.clear(cx));
                                        },
                                    ),
                                )
                            })
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .px(px(Theme::scaled(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .font_family(Theme::font_family())
                    .child(
                        Textarea::new(&self.text_input)
                            .h_full()
                            .readonly(true)
                            // No box, no border: the log is the panel's content,
                            // not a form field sitting inside it.
                            .appearance(false)
                            .bordered(false),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_off_blocks_all() {
        let log = DiagnosticLog::new(DiagnosticLogLevel::Off);
        log.log_error("test", "error msg");
        log.log_verbose("test", "verbose msg");
        assert_eq!(log.entry_count(), 0);
    }

    #[test]
    fn log_level_error_blocks_verbose() {
        let log = DiagnosticLog::new(DiagnosticLogLevel::Error);
        log.log_error("test", "error msg");
        log.log_verbose("test", "verbose msg");
        assert_eq!(log.entry_count(), 1);
        let entries = log.entries();
        assert_eq!(entries[0].level, DiagnosticEntryLevel::Error);
        assert_eq!(entries[0].message, "error msg");
    }

    #[test]
    fn log_level_verbose_allows_all() {
        let log = DiagnosticLog::new(DiagnosticLogLevel::Verbose);
        log.log_error("test", "error msg");
        log.log_verbose("test", "verbose msg");
        assert_eq!(log.entry_count(), 2);
    }

    #[test]
    fn log_format_line_is_grepable() {
        let entry = DiagnosticEntry {
            timestamp_ms: 12345,
            level: DiagnosticEntryLevel::Error,
            category: "connection".to_string(),
            message: "Failed to connect".to_string(),
        };
        let line = entry.format_line();
        assert!(line.contains("12345"));
        assert!(line.contains("ERR"));
        assert!(line.contains("[connection]"));
        assert!(line.contains("Failed to connect"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn hidden_badge_count_only_when_log_is_closed_with_entries() {
        assert_eq!(hidden_badge_count(false, 3), Some(3));
        assert_eq!(hidden_badge_count(false, 1), Some(1));
        assert_eq!(hidden_badge_count(true, 3), None);
        assert_eq!(hidden_badge_count(false, 0), None);
        assert_eq!(hidden_badge_count(true, 0), None);
    }

    #[test]
    fn log_clear_removes_entries() {
        let log = DiagnosticLog::new(DiagnosticLogLevel::Verbose);
        log.log_verbose("test", "msg1");
        log.log_verbose("test", "msg2");
        assert_eq!(log.entry_count(), 2);
        log.clear();
        assert_eq!(log.entry_count(), 0);
    }

    #[test]
    fn log_level_from_prefs() {
        assert_eq!(
            DiagnosticLogLevel::from_prefs(true, false),
            DiagnosticLogLevel::Verbose
        );
        assert_eq!(
            DiagnosticLogLevel::from_prefs(false, true),
            DiagnosticLogLevel::Error
        );
        assert_eq!(
            DiagnosticLogLevel::from_prefs(false, false),
            DiagnosticLogLevel::Off
        );
    }

    #[test]
    fn level_pref_converts_to_runtime_level() {
        use crate::preferences::DiagnosticLogLevelPref;
        assert_eq!(
            DiagnosticLogLevel::from(DiagnosticLogLevelPref::Off),
            DiagnosticLogLevel::Off
        );
        assert_eq!(
            DiagnosticLogLevel::from(DiagnosticLogLevelPref::Error),
            DiagnosticLogLevel::Error
        );
        assert_eq!(
            DiagnosticLogLevel::from(DiagnosticLogLevelPref::Verbose),
            DiagnosticLogLevel::Verbose
        );
    }

    #[test]
    fn wall_clock_timestamp_has_expected_shape() {
        let ts = wall_clock_timestamp();
        // yyyy-MM-dd HH:mm:ss.SSS  => 23 chars
        assert_eq!(ts.len(), 23, "unexpected timestamp: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], " ");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[19..20], ".");
    }

    #[test]
    fn file_backed_log_writes_lines() {
        let dir = std::env::temp_dir().join(format!("sqlly-log-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sqlly-diagnostics.log");
        let _ = std::fs::remove_file(&path);

        let log = DiagnosticLog::with_file(DiagnosticLogLevel::Verbose, path.clone());
        assert_eq!(log.file_path(), Some(path.clone()));
        log.log_verbose("startup", "hello world");
        log.log_error("connection", "boom\nsecond line");

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("VRB"));
        assert!(lines[0].contains("[startup] hello world"));
        assert!(lines[1].contains("ERR"));
        // Embedded newline must be escaped to keep one record per line.
        assert!(lines[1].contains("boom\\nsecond line"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_backed_log_respects_level_off() {
        let dir = std::env::temp_dir().join(format!("sqlly-log-off-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sqlly-diagnostics.log");
        let _ = std::fs::remove_file(&path);

        let log = DiagnosticLog::with_file(DiagnosticLogLevel::Off, path.clone());
        log.log_error("x", "should not be written");
        assert!(
            !path.exists(),
            "Off level must not create/write the log file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
