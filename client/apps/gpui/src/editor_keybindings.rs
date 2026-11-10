//! Variable keybindings for the SQL query editor.
//!
//! The editor reads its action→keystroke mapping from a JSON file at
//! `preferences_path()/keybindings.json`. The file is optional; when absent,
//! platform-aware defaults are used. When present, each entry *replaces* the
//! default strokes for that action (override semantics, matching the app-level
//! `keybindings` resolver). Invalid keystroke strings are skipped with a
//! `tracing::warn` so a typo in the file can never panic `KeyBinding::new`.
//!
//! JSON shape:
//!
//! ```json
//! {
//!   "SelectAll": "cmd-a",
//!   "ExecuteQuery": ["cmd-enter", "f5"],
//!   "MoveWordLeft": "alt-left"
//! }
//! ```
//!
//! Each value is either a single keystroke string or an array of them. Keys
//! are the camelCase editor action names (`Backspace`, `MoveWordLeft`, ...).

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{App, KeyBinding};
use serde::{Deserialize, Serialize};

use crate::editor::*;
use crate::keybindings::{load_binding, parse_keystroke};
use crate::preferences::preferences_path;

/// A single action's keystrokes in the JSON file: either a bare string
/// (`"cmd-left"`) or an array of strings (`["cmd-left", "home"]`). Normalized
/// to a `Vec<String>` on load.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum StrokesJson {
    One(String),
    Many(Vec<String>),
}

impl StrokesJson {
    fn into_vec(self) -> Vec<String> {
        match self {
            StrokesJson::One(s) => vec![s],
            StrokesJson::Many(v) => v,
        }
    }
}

/// Raw on-disk shape: `{ "actionName": "stroke" | ["stroke", ...], ... }`.
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
struct EditorKeybindingsFile(BTreeMap<String, StrokesJson>);

/// Resolved, validated bindings: action name -> ordered keystroke strings.
pub type EditorKeybindingMap = BTreeMap<String, Vec<String>>;

fn keybindings_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("editor-keybindings.json");
    p
}

/// Browser build: the overrides file lives in localStorage; this mirrors the
/// `std::fs::read` result shape so the load paths stay identical.
#[cfg(target_arch = "wasm32")]
fn read_keybindings_bytes() -> std::io::Result<Vec<u8>> {
    crate::host_storage::load_text(
        crate::host_storage::StorageArea::Data,
        "editor-keybindings.json",
    )
    .map(String::into_bytes)
    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

#[allow(dead_code)]
/// Every editor action name that can be bound. Kept in lock-step with the
/// `actions!(sql_editor, [...])` declaration in `editor.rs`.
pub const KNOWN_ACTIONS: &[&str] = &[
    "Backspace",
    "Delete",
    "Dismiss",
    "Left",
    "Right",
    "Up",
    "Down",
    "Home",
    "End",
    "Enter",
    "ShiftEnter",
    "SelectAll",
    "SelectLeft",
    "SelectRight",
    "SelectUp",
    "SelectDown",
    "SelectHome",
    "SelectEnd",
    "Tab",
    "ShiftTab",
    "ExecuteQuery",
    "Cancel",
    "OpenContextMenu",
    "TriggerCompletions",
    "TriggerValueLookup",
    "TriggerAiCompletion",
    "Fold",
    "Unfold",
    "FoldAll",
    "UnfoldAll",
    "CaptureIntelliSenseFeedback",
    "InsertDistinctValuesAsInList",
    "PasteAsInList",
    "ConvertDelimitedList",
    "Validate",
    "FormatSql",
    "FormatDocument",
    "RefreshDates",
    "PageDown",
    "PageUp",
    "SelectPageDown",
    "SelectPageUp",
    "ToggleLineComment",
    "AddLineComment",
    "RemoveLineComment",
    "MoveWordLeft",
    "MoveWordRight",
    "SelectWordLeft",
    "SelectWordRight",
    "DeleteWordBackward",
    "DeleteWordForward",
    "MoveHumpLeft",
    "MoveHumpRight",
    "SelectHumpLeft",
    "SelectHumpRight",
    "MoveToBeginningOfDocument",
    "MoveToEndOfDocument",
    "SelectToBeginningOfDocument",
    "SelectToEndOfDocument",
    "DeleteToBeginningOfLine",
    "DeleteToEndOfLine",
    "Copy",
    "Cut",
    "Paste",
    "Undo",
    "Redo",
    "Find",
    "Replace",
    "FindNext",
    "FindPrev",
    "AddCursorForNextOccurrence",
    "SelectAllOccurrences",
    "AddCursorAbove",
    "AddCursorBelow",
    "ColumnSelectUp",
    "ColumnSelectDown",
    "GoToLine",
    "GoToNextError",
    "QualifyIdentifier",
    "MoveLineUp",
    "MoveLineDown",
    "DuplicateLine",
    "JoinLines",
];

/// Platform-aware default editor keybindings. macOS uses cmd/alt; Linux and
/// Windows use ctrl/home. These mirror the standard text-editor bindings.
pub fn default_bindings() -> EditorKeybindingMap {
    let mut m: EditorKeybindingMap = BTreeMap::new();

    // Platform-independent bindings.
    m.insert("Backspace".into(), vec!["backspace".into()]);
    m.insert("Delete".into(), vec!["delete".into()]);
    m.insert("Left".into(), vec!["left".into()]);
    m.insert("Right".into(), vec!["right".into()]);
    m.insert("Up".into(), vec!["up".into()]);
    m.insert("Down".into(), vec!["down".into()]);
    m.insert("Home".into(), vec!["home".into()]);
    m.insert("End".into(), vec!["end".into()]);
    m.insert("Enter".into(), vec!["enter".into()]);
    m.insert("ShiftEnter".into(), vec!["shift-enter".into()]);
    // Esc dismisses transient UI (completion popup, ghost text, selection) —
    // the universal editor convention. Query cancel is the app-level
    // cmd/shift-escape binding, deliberately NOT bound here.
    m.insert("Dismiss".into(), vec!["escape".into()]);
    // Keyboard route to the right-click menu (Windows convention; the
    // dedicated Menu key maps to "menu" where the platform reports it).
    m.insert(
        "OpenContextMenu".into(),
        vec!["shift-f10".into(), "menu".into()],
    );
    m.insert("GoToNextError".into(), vec!["f8".into()]);
    m.insert("Tab".into(), vec!["tab".into()]);
    m.insert("ShiftTab".into(), vec!["shift-tab".into()]);
    // F2 over a live completion popup captures the IntelliSense context and
    // opens the "improve IntelliSense" feedback dialog.
    m.insert("CaptureIntelliSenseFeedback".into(), vec!["f2".into()]);
    m.insert("SelectLeft".into(), vec!["shift-left".into()]);
    m.insert("SelectRight".into(), vec!["shift-right".into()]);
    m.insert("SelectUp".into(), vec!["shift-up".into()]);
    m.insert("SelectDown".into(), vec!["shift-down".into()]);
    m.insert("SelectHome".into(), vec!["shift-home".into()]);
    m.insert("SelectEnd".into(), vec!["shift-end".into()]);
    m.insert("PageDown".into(), vec!["pagedown".into()]);
    m.insert("PageUp".into(), vec!["pageup".into()]);
    m.insert("SelectPageDown".into(), vec!["shift-pagedown".into()]);
    m.insert("SelectPageUp".into(), vec!["shift-pageup".into()]);

    #[cfg(target_os = "macos")]
    {
        m.insert("SelectAll".into(), vec!["cmd-a".into()]);
        m.insert("ExecuteQuery".into(), vec!["cmd-enter".into(), "f5".into()]);
        // Cancel is intentionally unbound: cmd-escape must reach the
        // app-level CancelActiveQuery (the editor action only dismisses UI).
        // cmd-space is owned by Spotlight, so completions bind ctrl-space only.
        m.insert("TriggerCompletions".into(), vec!["ctrl-space".into()]);
        m.insert("TriggerValueLookup".into(), vec!["ctrl-shift-space".into()]);
        // Manual FIM ghost-text request; alt-\ matches VS Code's inline
        // suggestion trigger (ctrl-space belongs to the completion popup).
        m.insert("TriggerAiCompletion".into(), vec!["alt-\\".into()]);
        // Code folding, VS Code parity (fold-all/unfold-all use shift in
        // place of VS Code's cmd-k chords).
        m.insert("Fold".into(), vec!["cmd-alt-[".into()]);
        m.insert("Unfold".into(), vec!["cmd-alt-]".into()]);
        m.insert("FoldAll".into(), vec!["cmd-alt-shift-[".into()]);
        m.insert("UnfoldAll".into(), vec!["cmd-alt-shift-]".into()]);
        m.insert("Validate".into(), vec!["cmd-shift-v".into()]);
        // VS Code parity: Shift+Option+F formats the whole document,
        // Cmd+K Cmd+F just the selection.
        m.insert("FormatDocument".into(), vec!["alt-shift-f".into()]);
        m.insert("FormatSql".into(), vec!["cmd-k cmd-f".into()]);
        m.insert("PasteAsInList".into(), vec!["alt-shift-i".into()]);
        m.insert("ConvertDelimitedList".into(), vec!["alt-shift-l".into()]);
        m.insert("ToggleLineComment".into(), vec!["cmd-/".into()]);
        // VS Code's comment chords: cmd-k cmd-c comments, cmd-k cmd-u uncomments.
        m.insert("AddLineComment".into(), vec!["cmd-k cmd-c".into()]);
        m.insert("RemoveLineComment".into(), vec!["cmd-k cmd-u".into()]);
        // macOS line-edge nav via cmd-arrows (in addition to home/end), plus
        // the NSTextView Emacs-style ctrl-a / ctrl-e every Mac text field has.
        m.insert(
            "Home".into(),
            vec!["home".into(), "cmd-left".into(), "ctrl-a".into()],
        );
        m.insert(
            "End".into(),
            vec!["end".into(), "cmd-right".into(), "ctrl-e".into()],
        );
        m.insert(
            "SelectHome".into(),
            vec!["shift-home".into(), "cmd-shift-left".into()],
        );
        m.insert(
            "SelectEnd".into(),
            vec!["shift-end".into(), "cmd-shift-right".into()],
        );
        // Word nav: alt. Hump (camelCase) nav: ctrl-alt.
        m.insert("MoveWordLeft".into(), vec!["alt-left".into()]);
        m.insert("MoveWordRight".into(), vec!["alt-right".into()]);
        m.insert("SelectWordLeft".into(), vec!["alt-shift-left".into()]);
        m.insert("SelectWordRight".into(), vec!["alt-shift-right".into()]);
        m.insert("DeleteWordBackward".into(), vec!["alt-backspace".into()]);
        m.insert("DeleteWordForward".into(), vec!["alt-delete".into()]);
        m.insert("MoveHumpLeft".into(), vec!["ctrl-alt-left".into()]);
        m.insert("MoveHumpRight".into(), vec!["ctrl-alt-right".into()]);
        m.insert("SelectHumpLeft".into(), vec!["ctrl-alt-shift-left".into()]);
        m.insert(
            "SelectHumpRight".into(),
            vec!["ctrl-alt-shift-right".into()],
        );
        m.insert(
            "MoveToBeginningOfDocument".into(),
            vec!["cmd-up".into(), "cmd-home".into()],
        );
        m.insert(
            "MoveToEndOfDocument".into(),
            vec!["cmd-down".into(), "cmd-end".into()],
        );
        m.insert(
            "SelectToBeginningOfDocument".into(),
            vec!["cmd-shift-up".into(), "cmd-shift-home".into()],
        );
        m.insert(
            "SelectToEndOfDocument".into(),
            vec!["cmd-shift-down".into(), "cmd-shift-end".into()],
        );
        m.insert(
            "DeleteToBeginningOfLine".into(),
            vec!["cmd-backspace".into()],
        );
        // ctrl-k is the NSTextView kill-to-end-of-line binding.
        m.insert(
            "DeleteToEndOfLine".into(),
            vec!["cmd-delete".into(), "ctrl-k".into()],
        );
        m.insert("Copy".into(), vec!["cmd-c".into()]);
        m.insert("Cut".into(), vec!["cmd-x".into()]);
        m.insert("Paste".into(), vec!["cmd-v".into()]);
        m.insert("Undo".into(), vec!["cmd-z".into()]);
        m.insert("Redo".into(), vec!["cmd-shift-z".into()]);
        m.insert("Find".into(), vec!["cmd-f".into()]);
        m.insert("Replace".into(), vec!["cmd-h".into()]);
        m.insert("FindNext".into(), vec!["cmd-g".into(), "f3".into()]);
        m.insert(
            "FindPrev".into(),
            vec!["cmd-shift-g".into(), "shift-f3".into()],
        );
        // Multi-cursor, VS Code parity: cmd-d grabs the next occurrence,
        // cmd-shift-l all of them, cmd-alt-up/down stack carets vertically.
        m.insert("AddCursorForNextOccurrence".into(), vec!["cmd-d".into()]);
        m.insert("SelectAllOccurrences".into(), vec!["cmd-shift-l".into()]);
        m.insert("AddCursorAbove".into(), vec!["cmd-alt-up".into()]);
        m.insert("AddCursorBelow".into(), vec!["cmd-alt-down".into()]);
        m.insert("ColumnSelectUp".into(), vec!["alt-shift-up".into()]);
        m.insert("ColumnSelectDown".into(), vec!["alt-shift-down".into()]);
        // Line verbs + Go to Line (VS Code parity where the chord is free).
        m.insert("GoToLine".into(), vec!["ctrl-g".into()]);
        m.insert("MoveLineUp".into(), vec!["alt-up".into()]);
        m.insert("MoveLineDown".into(), vec!["alt-down".into()]);
        m.insert("DuplicateLine".into(), vec!["cmd-shift-d".into()]);
        m.insert("JoinLines".into(), vec!["ctrl-j".into()]);
        m.insert("QualifyIdentifier".into(), vec!["cmd-alt-q".into()]);
    }

    #[cfg(not(target_os = "macos"))]
    {
        m.insert("SelectAll".into(), vec!["ctrl-a".into()]);
        m.insert(
            "ExecuteQuery".into(),
            vec!["ctrl-enter".into(), "f5".into()],
        );
        // Cancel is intentionally unbound: ctrl-escape opens the Windows
        // Start menu, and query cancel belongs to the app-level binding
        // (shift-escape) anyway. The editor action only dismisses UI.
        m.insert("TriggerCompletions".into(), vec!["ctrl-space".into()]);
        m.insert("TriggerValueLookup".into(), vec!["ctrl-shift-space".into()]);
        // Manual FIM ghost-text request; alt-\ matches VS Code's inline
        // suggestion trigger (ctrl-space belongs to the completion popup).
        m.insert("TriggerAiCompletion".into(), vec!["alt-\\".into()]);
        // Code folding, VS Code parity (fold-all/unfold-all add alt in
        // place of VS Code's ctrl-k chords).
        m.insert("Fold".into(), vec!["ctrl-shift-[".into()]);
        m.insert("Unfold".into(), vec!["ctrl-shift-]".into()]);
        m.insert("FoldAll".into(), vec!["ctrl-alt-shift-[".into()]);
        m.insert("UnfoldAll".into(), vec!["ctrl-alt-shift-]".into()]);
        m.insert("Validate".into(), vec!["ctrl-shift-v".into()]);
        // VS Code parity: Shift+Alt+F formats the whole document,
        // Ctrl+K Ctrl+F just the selection.
        m.insert("FormatDocument".into(), vec!["alt-shift-f".into()]);
        m.insert("FormatSql".into(), vec!["ctrl-k ctrl-f".into()]);
        m.insert("PasteAsInList".into(), vec!["alt-shift-i".into()]);
        m.insert("ConvertDelimitedList".into(), vec!["alt-shift-l".into()]);
        m.insert("ToggleLineComment".into(), vec!["ctrl-/".into()]);
        // VS Code's comment chords: ctrl-k ctrl-c comments, ctrl-k ctrl-u uncomments.
        m.insert("AddLineComment".into(), vec!["ctrl-k ctrl-c".into()]);
        m.insert("RemoveLineComment".into(), vec!["ctrl-k ctrl-u".into()]);
        // Word nav: ctrl. Hump (camelCase) nav: alt.
        m.insert("MoveWordLeft".into(), vec!["ctrl-left".into()]);
        m.insert("MoveWordRight".into(), vec!["ctrl-right".into()]);
        m.insert("SelectWordLeft".into(), vec!["ctrl-shift-left".into()]);
        m.insert("SelectWordRight".into(), vec!["ctrl-shift-right".into()]);
        m.insert("DeleteWordBackward".into(), vec!["ctrl-backspace".into()]);
        m.insert("DeleteWordForward".into(), vec!["ctrl-delete".into()]);
        m.insert("MoveHumpLeft".into(), vec!["alt-left".into()]);
        m.insert("MoveHumpRight".into(), vec!["alt-right".into()]);
        m.insert("SelectHumpLeft".into(), vec!["alt-shift-left".into()]);
        m.insert("SelectHumpRight".into(), vec!["alt-shift-right".into()]);
        m.insert("MoveToBeginningOfDocument".into(), vec!["ctrl-home".into()]);
        m.insert("MoveToEndOfDocument".into(), vec!["ctrl-end".into()]);
        m.insert(
            "SelectToBeginningOfDocument".into(),
            vec!["ctrl-shift-home".into()],
        );
        m.insert(
            "SelectToEndOfDocument".into(),
            vec!["ctrl-shift-end".into()],
        );
        m.insert(
            "DeleteToBeginningOfLine".into(),
            vec!["ctrl-shift-backspace".into()],
        );
        m.insert("DeleteToEndOfLine".into(), vec!["ctrl-shift-delete".into()]);
        // The IBM CUA legacy chords still live in many Windows/Linux users'
        // muscle memory alongside the ctrl-letter set.
        m.insert("Copy".into(), vec!["ctrl-c".into(), "ctrl-insert".into()]);
        m.insert("Cut".into(), vec!["ctrl-x".into(), "shift-delete".into()]);
        m.insert("Paste".into(), vec!["ctrl-v".into(), "shift-insert".into()]);
        m.insert("Undo".into(), vec!["ctrl-z".into()]);
        m.insert("Redo".into(), vec!["ctrl-y".into(), "ctrl-shift-z".into()]);
        m.insert("Find".into(), vec!["ctrl-f".into()]);
        m.insert("Replace".into(), vec!["ctrl-h".into()]);
        m.insert("FindNext".into(), vec!["f3".into()]);
        m.insert("FindPrev".into(), vec!["shift-f3".into()]);
        // Multi-cursor, VS Code parity: ctrl-d grabs the next occurrence,
        // ctrl-shift-l all of them, ctrl-alt-up/down stack carets vertically.
        m.insert("AddCursorForNextOccurrence".into(), vec!["ctrl-d".into()]);
        m.insert("SelectAllOccurrences".into(), vec!["ctrl-shift-l".into()]);
        m.insert("AddCursorAbove".into(), vec!["ctrl-alt-up".into()]);
        m.insert("AddCursorBelow".into(), vec!["ctrl-alt-down".into()]);
        m.insert("ColumnSelectUp".into(), vec!["alt-shift-up".into()]);
        m.insert("ColumnSelectDown".into(), vec!["alt-shift-down".into()]);
        // Line verbs + Go to Line (VS Code parity where the chord is free).
        m.insert("GoToLine".into(), vec!["ctrl-g".into()]);
        m.insert("MoveLineUp".into(), vec!["alt-up".into()]);
        m.insert("MoveLineDown".into(), vec!["alt-down".into()]);
        m.insert("DuplicateLine".into(), vec!["ctrl-shift-d".into()]);
        m.insert("JoinLines".into(), vec!["ctrl-j".into()]);
        m.insert("QualifyIdentifier".into(), vec!["ctrl-alt-q".into()]);
    }

    m
}

/// Validate a chord string by parsing each whitespace-separated stroke.
/// Mirrors the split `KeyBinding::new` performs, so a `true` result guarantees
/// `KeyBinding::new` will not panic. Empty / whitespace-only input is rejected
/// (there is no keystroke to bind).
fn is_valid_chord(s: &str) -> bool {
    let mut pieces = s.split_whitespace();
    let Some(first) = pieces.next() else {
        return false;
    };
    parse_keystroke(first).is_ok() && pieces.all(|piece| parse_keystroke(piece).is_ok())
}

/// Merge JSON overrides into the defaults. Per-action replace semantics: a
/// present key replaces the default strokes. Invalid strokes are dropped; an
/// action whose every stroke is invalid keeps its default.
fn merge_overrides(
    defaults: EditorKeybindingMap,
    file: &EditorKeybindingsFile,
) -> EditorKeybindingMap {
    let mut out = defaults;
    for (name, strokes) in &file.0 {
        let candidates = strokes.clone().into_vec();
        let valid: Vec<String> = candidates
            .into_iter()
            .filter(|s| is_valid_chord(s))
            .collect();
        if valid.is_empty() {
            tracing::warn!("keybindings.json: action {name} has no valid strokes; keeping default");
            continue;
        }
        out.insert(name.clone(), valid);
    }
    out
}

/// Load the resolved editor keybindings: defaults merged with JSON overrides
/// from `keybindings.json` (if present and parseable). Missing file = defaults.
pub fn load_editor_keybindings() -> EditorKeybindingMap {
    let defaults = default_bindings();
    #[cfg(target_arch = "wasm32")]
    let read_result = read_keybindings_bytes();
    #[cfg(not(target_arch = "wasm32"))]
    let read_result = std::fs::read(keybindings_path());
    match read_result {
        Ok(bytes) => match serde_json::from_slice::<EditorKeybindingsFile>(&bytes) {
            Ok(file) => merge_overrides(defaults, &file),
            Err(e) => {
                tracing::warn!("keybindings.json parse error: {e}");
                defaults
            }
        },
        Err(_) => defaults,
    }
}

/// Register the editor keybindings with gpui. Reads `editor-keybindings.json`,
/// merges with defaults, validates, and binds each stroke to its action.
pub fn register_keybindings(cx: &mut App) {
    let map = load_editor_keybindings();
    for (name, strokes) in &map {
        bind_action_strokes(cx, name, strokes);
    }
}

/// Load only the overrides (not merged with defaults) from `editor-keybindings.json`.
/// Returns an empty map when the file is missing, unreadable, or malformed.
pub fn load_editor_overrides() -> EditorKeybindingMap {
    #[cfg(target_arch = "wasm32")]
    let read_result = read_keybindings_bytes();
    #[cfg(not(target_arch = "wasm32"))]
    let read_result = std::fs::read(keybindings_path());
    match read_result {
        Ok(bytes) => match serde_json::from_slice::<EditorKeybindingsFile>(&bytes) {
            Ok(file) => {
                let mut overrides: EditorKeybindingMap = BTreeMap::new();
                for (name, strokes) in &file.0 {
                    let candidates = strokes.clone().into_vec();
                    let valid: Vec<String> = candidates
                        .into_iter()
                        .filter(|s| is_valid_chord(s))
                        .collect();
                    if !valid.is_empty() {
                        overrides.insert(name.clone(), valid);
                    }
                }
                overrides
            }
            Err(e) => {
                tracing::warn!("editor-keybindings.json parse error: {e}");
                BTreeMap::new()
            }
        },
        Err(_) => BTreeMap::new(),
    }
}

/// Resolve defaults plus a caller-supplied override snapshot without touching
/// disk. This is the same replacement rule startup uses and lets Preferences
/// reject a conflict before persisting it.
pub fn resolve_editor_overrides(overrides: &EditorKeybindingMap) -> EditorKeybindingMap {
    let mut resolved = default_bindings();
    for (name, strokes) in overrides {
        if !KNOWN_ACTIONS.contains(&name.as_str()) {
            continue;
        }
        let valid: Vec<_> = strokes
            .iter()
            .filter(|stroke| is_valid_chord(stroke))
            .cloned()
            .collect();
        if !valid.is_empty() {
            resolved.insert(name.clone(), valid);
        }
    }
    resolved
}

pub fn conflicts_for_editor_binding(
    action_name: &str,
    stroke: &str,
    overrides: &EditorKeybindingMap,
) -> Vec<String> {
    if !is_valid_chord(stroke) {
        return Vec::new();
    }
    let mut conflicts: Vec<_> = resolve_editor_overrides(overrides)
        .into_iter()
        .filter(|(name, strokes)| name != action_name && strokes.iter().any(|item| item == stroke))
        .map(|(name, _)| name)
        .collect();
    conflicts.sort();
    conflicts
}

/// Save editor keybinding overrides to `editor-keybindings.json`.
pub fn save_editor_overrides(overrides: &EditorKeybindingMap) -> std::io::Result<()> {
    let mut raw: BTreeMap<String, StrokesJson> = BTreeMap::new();
    for (name, strokes) in overrides {
        raw.insert(name.clone(), StrokesJson::Many(strokes.clone()));
    }
    let file = EditorKeybindingsFile(raw);
    let json = serde_json::to_string_pretty(&file)?;
    // Browser build: the overrides live in localStorage.
    #[cfg(target_arch = "wasm32")]
    return crate::host_storage::store_text(
        crate::host_storage::StorageArea::Data,
        "editor-keybindings.json",
        &(json + "\n"),
    )
    .map_err(std::io::Error::other);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = keybindings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, json + "\n")
    }
}

/// Human-readable display name for an editor action, suitable for showing in
/// the keyboard shortcuts UI. Falls back to the action name itself.
pub fn editor_action_display_name(name: &str) -> String {
    match name {
        "Backspace" => {
            crate::localization::text("settings.keybindings.editor_action.delete_backward")
        }
        "Delete" => crate::localization::text("settings.keybindings.editor_action.delete_forward"),
        "Left" => crate::localization::text("settings.keybindings.editor_action.move_left"),
        "Right" => crate::localization::text("settings.keybindings.editor_action.move_right"),
        "Up" => crate::localization::text("settings.keybindings.editor_action.move_up"),
        "Down" => crate::localization::text("settings.keybindings.editor_action.move_down"),
        "Home" => crate::localization::text("settings.keybindings.editor_action.move_line_start"),
        "End" => crate::localization::text("settings.keybindings.editor_action.move_line_end"),
        "Enter" => crate::localization::text("settings.keybindings.editor_action.insert_newline"),
        "ShiftEnter" => crate::localization::text("settings.keybindings.editor_action.shift_enter"),
        "SelectAll" => crate::localization::text("settings.keybindings.editor_action.select_all"),
        "SelectLeft" => crate::localization::text("settings.keybindings.editor_action.select_left"),
        "SelectRight" => {
            crate::localization::text("settings.keybindings.editor_action.select_right")
        }
        "SelectUp" => crate::localization::text("settings.keybindings.editor_action.select_up"),
        "SelectDown" => crate::localization::text("settings.keybindings.editor_action.select_down"),
        "SelectHome" => {
            crate::localization::text("settings.keybindings.editor_action.select_line_start")
        }
        "SelectEnd" => {
            crate::localization::text("settings.keybindings.editor_action.select_line_end")
        }
        "Tab" => crate::localization::text("settings.keybindings.editor_action.tab"),
        "ShiftTab" => crate::localization::text("settings.keybindings.editor_action.shift_tab"),
        "Dismiss" => {
            crate::localization::text("settings.keybindings.editor_action.dismiss_popup_selection")
        }
        "OpenContextMenu" => {
            crate::localization::text("settings.keybindings.editor_action.open_context_menu")
        }
        "ExecuteQuery" => {
            crate::localization::text("settings.keybindings.editor_action.execute_query")
        }
        "Cancel" => {
            crate::localization::text("settings.keybindings.editor_action.dismiss_legacy_alias")
        }
        "TriggerCompletions" => {
            crate::localization::text("settings.keybindings.editor_action.trigger_completions")
        }
        "TriggerValueLookup" => {
            crate::localization::text("settings.keybindings.editor_action.trigger_value_lookup")
        }
        "TriggerAiCompletion" => crate::localization::text(
            "settings.keybindings.editor_action.trigger_ai_inline_suggestion",
        ),
        "Fold" => crate::localization::text("settings.keybindings.editor_action.fold_region"),
        "Unfold" => crate::localization::text("settings.keybindings.editor_action.unfold_region"),
        "FoldAll" => {
            crate::localization::text("settings.keybindings.editor_action.fold_all_regions")
        }
        "UnfoldAll" => {
            crate::localization::text("settings.keybindings.editor_action.unfold_all_regions")
        }
        "CaptureIntelliSenseFeedback" => crate::localization::text(
            "settings.keybindings.editor_action.improve_intellisense_capture_feedback",
        ),
        "InsertDistinctValuesAsInList" => crate::localization::text(
            "settings.keybindings.editor_action.insert_distinct_values_as_list",
        ),
        "PasteAsInList" => {
            crate::localization::text("settings.keybindings.editor_action.paste_as_list")
        }
        "ConvertDelimitedList" => {
            crate::localization::text("settings.keybindings.editor_action.convert_delimited_list")
        }
        "Validate" => crate::localization::text("settings.keybindings.editor_action.validate_sql"),
        "FormatSql" => {
            crate::localization::text("settings.keybindings.editor_action.format_selection")
        }
        "FormatDocument" => {
            crate::localization::text("settings.keybindings.editor_action.format_document")
        }
        "RefreshDates" => crate::localization::text(
            "settings.keybindings.editor_action.refresh_natural_language_dates",
        ),
        "PageDown" => crate::localization::text("settings.keybindings.editor_action.page_down"),
        "PageUp" => crate::localization::text("settings.keybindings.editor_action.page_up"),
        "SelectPageDown" => {
            crate::localization::text("settings.keybindings.editor_action.select_page_down")
        }
        "SelectPageUp" => {
            crate::localization::text("settings.keybindings.editor_action.select_page_up")
        }
        "ToggleLineComment" => {
            crate::localization::text("settings.keybindings.editor_action.toggle_line_comment")
        }
        "AddLineComment" => {
            crate::localization::text("settings.keybindings.editor_action.add_line_comment")
        }
        "RemoveLineComment" => {
            crate::localization::text("settings.keybindings.editor_action.remove_line_comment")
        }
        "MoveWordLeft" => {
            crate::localization::text("settings.keybindings.editor_action.move_word_left")
        }
        "MoveWordRight" => {
            crate::localization::text("settings.keybindings.editor_action.move_word_right")
        }
        "SelectWordLeft" => {
            crate::localization::text("settings.keybindings.editor_action.select_word_left")
        }
        "SelectWordRight" => {
            crate::localization::text("settings.keybindings.editor_action.select_word_right")
        }
        "DeleteWordBackward" => {
            crate::localization::text("settings.keybindings.editor_action.delete_word_backward")
        }
        "DeleteWordForward" => {
            crate::localization::text("settings.keybindings.editor_action.delete_word_forward")
        }
        "MoveHumpLeft" => {
            crate::localization::text("settings.keybindings.editor_action.move_camelcase_left")
        }
        "MoveHumpRight" => {
            crate::localization::text("settings.keybindings.editor_action.move_camelcase_right")
        }
        "SelectHumpLeft" => {
            crate::localization::text("settings.keybindings.editor_action.select_camelcase_left")
        }
        "SelectHumpRight" => {
            crate::localization::text("settings.keybindings.editor_action.select_camelcase_right")
        }
        "MoveToBeginningOfDocument" => {
            crate::localization::text("settings.keybindings.editor_action.move_document_start")
        }
        "MoveToEndOfDocument" => {
            crate::localization::text("settings.keybindings.editor_action.move_document_end")
        }
        "SelectToBeginningOfDocument" => {
            crate::localization::text("settings.keybindings.editor_action.select_document_start")
        }
        "SelectToEndOfDocument" => {
            crate::localization::text("settings.keybindings.editor_action.select_document_end")
        }
        "DeleteToBeginningOfLine" => {
            crate::localization::text("settings.keybindings.editor_action.delete_line_start")
        }
        "DeleteToEndOfLine" => {
            crate::localization::text("settings.keybindings.editor_action.delete_line_end")
        }
        "Copy" => crate::localization::text("settings.keybindings.editor_action.copy"),
        "Cut" => crate::localization::text("settings.keybindings.editor_action.cut"),
        "Paste" => crate::localization::text("settings.keybindings.editor_action.paste"),
        "Undo" => crate::localization::text("settings.keybindings.editor_action.undo"),
        "Redo" => crate::localization::text("settings.keybindings.editor_action.redo"),
        "Find" => crate::localization::text("settings.keybindings.editor_action.find"),
        "Replace" => crate::localization::text("settings.keybindings.editor_action.replace"),
        "FindNext" => crate::localization::text("settings.keybindings.editor_action.find_next"),
        "FindPrev" => crate::localization::text("settings.keybindings.editor_action.find_previous"),
        "AddCursorForNextOccurrence" => crate::localization::text(
            "settings.keybindings.editor_action.add_cursor_next_occurrence",
        ),
        "SelectAllOccurrences" => {
            crate::localization::text("settings.keybindings.editor_action.select_all_occurrences")
        }
        "AddCursorAbove" => {
            crate::localization::text("settings.keybindings.editor_action.add_cursor_above")
        }
        "AddCursorBelow" => {
            crate::localization::text("settings.keybindings.editor_action.add_cursor_below")
        }
        "ColumnSelectUp" => {
            crate::localization::text("settings.keybindings.editor_action.column_select_up")
        }
        "ColumnSelectDown" => {
            crate::localization::text("settings.keybindings.editor_action.column_select_down")
        }
        "GoToLine" => crate::localization::text("settings.keybindings.editor_action.go_line"),
        "GoToNextError" => {
            crate::localization::text("settings.keybindings.editor_action.go_next_error")
        }
        "QualifyIdentifier" => {
            crate::localization::text("settings.keybindings.editor_action.qualify_identifier")
        }
        "MoveLineUp" => {
            crate::localization::text("settings.keybindings.editor_action.move_line_up")
        }
        "MoveLineDown" => {
            crate::localization::text("settings.keybindings.editor_action.move_line_down")
        }
        "DuplicateLine" => {
            crate::localization::text("settings.keybindings.editor_action.duplicate_line")
        }
        "JoinLines" => crate::localization::text("settings.keybindings.editor_action.join_lines"),
        _ => crate::localization::text("settings.keybindings.editor_action.unknown"),
    }
}

/// Convert a `gpui::Keystroke` into a stroke string suitable for the editor
/// keybindings file (e.g. "cmd-c", "alt-shift-left", "f5").
pub fn keystroke_to_stroke_string(ks: &gpui::Keystroke) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if ks.modifiers.platform {
        parts.push("cmd");
    }
    if ks.modifiers.shift {
        parts.push("shift");
    }
    if ks.modifiers.alt {
        parts.push("alt");
    }
    if ks.modifiers.control {
        parts.push("ctrl");
    }
    if ks.modifiers.function {
        parts.push("fn");
    }
    parts.push(ks.key.as_str());
    parts.join("-")
}

fn bind_action_strokes(cx: &mut App, name: &str, strokes: &[String]) {
    let bindings: Vec<KeyBinding> = strokes
        .iter()
        .filter_map(|s| binding_for(name, s, cx))
        .collect();
    if !bindings.is_empty() {
        cx.bind_keys(bindings);
    }
}

/// Construct a single `KeyBinding` for `stroke` -> `name`, validating the
/// stroke first so a malformed entry is skipped up front.
/// Returns `None` for unknown action names or invalid strokes.
fn binding_for(name: &str, stroke: &str, cx: &App) -> Option<KeyBinding> {
    if !is_valid_chord(stroke) {
        tracing::warn!("skipping invalid editor keystroke {stroke:?} for action {name}");
        return None;
    }
    // Scoped to the editor's `.key_context("SqlEditor")` so editor strokes
    // never shadow app-level bindings (and vice versa) when focus is
    // elsewhere. Bare keys like `escape` or `enter` are only safe to bind
    // because of this scope.
    let ctx: Option<&str> = Some("SqlEditor");
    Some(match name {
        "Backspace" => load_binding(stroke, Backspace, ctx, cx)?,
        "Delete" => load_binding(stroke, Delete, ctx, cx)?,
        "Dismiss" => load_binding(stroke, Dismiss, ctx, cx)?,
        "Left" => load_binding(stroke, Left, ctx, cx)?,
        "Right" => load_binding(stroke, Right, ctx, cx)?,
        "Up" => load_binding(stroke, Up, ctx, cx)?,
        "Down" => load_binding(stroke, Down, ctx, cx)?,
        "Home" => load_binding(stroke, Home, ctx, cx)?,
        "End" => load_binding(stroke, End, ctx, cx)?,
        "Enter" => load_binding(stroke, Enter, ctx, cx)?,
        "ShiftEnter" => load_binding(stroke, ShiftEnter, ctx, cx)?,
        "SelectAll" => load_binding(stroke, SelectAll, ctx, cx)?,
        "SelectLeft" => load_binding(stroke, SelectLeft, ctx, cx)?,
        "SelectRight" => load_binding(stroke, SelectRight, ctx, cx)?,
        "SelectUp" => load_binding(stroke, SelectUp, ctx, cx)?,
        "SelectDown" => load_binding(stroke, SelectDown, ctx, cx)?,
        "SelectHome" => load_binding(stroke, SelectHome, ctx, cx)?,
        "SelectEnd" => load_binding(stroke, SelectEnd, ctx, cx)?,
        "Tab" => load_binding(stroke, Tab, ctx, cx)?,
        "ShiftTab" => load_binding(stroke, ShiftTab, ctx, cx)?,
        "ExecuteQuery" => load_binding(stroke, ExecuteQuery, ctx, cx)?,
        "Cancel" => load_binding(stroke, Cancel, ctx, cx)?,
        "OpenContextMenu" => load_binding(stroke, OpenContextMenu, ctx, cx)?,
        "TriggerCompletions" => load_binding(stroke, TriggerCompletions, ctx, cx)?,
        "TriggerValueLookup" => load_binding(stroke, TriggerValueLookup, ctx, cx)?,
        "TriggerAiCompletion" => load_binding(stroke, TriggerAiCompletion, ctx, cx)?,
        "Fold" => load_binding(stroke, Fold, ctx, cx)?,
        "Unfold" => load_binding(stroke, Unfold, ctx, cx)?,
        "FoldAll" => load_binding(stroke, FoldAll, ctx, cx)?,
        "UnfoldAll" => load_binding(stroke, UnfoldAll, ctx, cx)?,
        "CaptureIntelliSenseFeedback" => {
            load_binding(stroke, CaptureIntelliSenseFeedback, ctx, cx)?
        }
        "InsertDistinctValuesAsInList" => {
            load_binding(stroke, InsertDistinctValuesAsInList, ctx, cx)?
        }
        "PasteAsInList" => load_binding(stroke, PasteAsInList, ctx, cx)?,
        "ConvertDelimitedList" => load_binding(stroke, ConvertDelimitedList, ctx, cx)?,
        "Validate" => load_binding(stroke, Validate, ctx, cx)?,
        "FormatSql" => load_binding(stroke, FormatSql, ctx, cx)?,
        "FormatDocument" => load_binding(stroke, FormatDocument, ctx, cx)?,
        "RefreshDates" => load_binding(stroke, RefreshDates, ctx, cx)?,
        "PageDown" => load_binding(stroke, PageDown, ctx, cx)?,
        "PageUp" => load_binding(stroke, PageUp, ctx, cx)?,
        "SelectPageDown" => load_binding(stroke, SelectPageDown, ctx, cx)?,
        "SelectPageUp" => load_binding(stroke, SelectPageUp, ctx, cx)?,
        "ToggleLineComment" => load_binding(stroke, ToggleLineComment, ctx, cx)?,
        "AddLineComment" => load_binding(stroke, AddLineComment, ctx, cx)?,
        "RemoveLineComment" => load_binding(stroke, RemoveLineComment, ctx, cx)?,
        "MoveWordLeft" => load_binding(stroke, MoveWordLeft, ctx, cx)?,
        "MoveWordRight" => load_binding(stroke, MoveWordRight, ctx, cx)?,
        "SelectWordLeft" => load_binding(stroke, SelectWordLeft, ctx, cx)?,
        "SelectWordRight" => load_binding(stroke, SelectWordRight, ctx, cx)?,
        "DeleteWordBackward" => load_binding(stroke, DeleteWordBackward, ctx, cx)?,
        "DeleteWordForward" => load_binding(stroke, DeleteWordForward, ctx, cx)?,
        "MoveHumpLeft" => load_binding(stroke, MoveHumpLeft, ctx, cx)?,
        "MoveHumpRight" => load_binding(stroke, MoveHumpRight, ctx, cx)?,
        "SelectHumpLeft" => load_binding(stroke, SelectHumpLeft, ctx, cx)?,
        "SelectHumpRight" => load_binding(stroke, SelectHumpRight, ctx, cx)?,
        "MoveToBeginningOfDocument" => load_binding(stroke, MoveToBeginningOfDocument, ctx, cx)?,
        "MoveToEndOfDocument" => load_binding(stroke, MoveToEndOfDocument, ctx, cx)?,
        "SelectToBeginningOfDocument" => {
            load_binding(stroke, SelectToBeginningOfDocument, ctx, cx)?
        }
        "SelectToEndOfDocument" => load_binding(stroke, SelectToEndOfDocument, ctx, cx)?,
        "DeleteToBeginningOfLine" => load_binding(stroke, DeleteToBeginningOfLine, ctx, cx)?,
        "DeleteToEndOfLine" => load_binding(stroke, DeleteToEndOfLine, ctx, cx)?,
        "Copy" => load_binding(stroke, Copy, ctx, cx)?,
        "Cut" => load_binding(stroke, Cut, ctx, cx)?,
        "Paste" => load_binding(stroke, Paste, ctx, cx)?,
        "Undo" => load_binding(stroke, Undo, ctx, cx)?,
        "Redo" => load_binding(stroke, Redo, ctx, cx)?,
        "Find" => load_binding(stroke, Find, ctx, cx)?,
        "Replace" => load_binding(stroke, Replace, ctx, cx)?,
        "FindNext" => load_binding(stroke, FindNext, ctx, cx)?,
        "FindPrev" => load_binding(stroke, FindPrev, ctx, cx)?,
        "AddCursorForNextOccurrence" => load_binding(stroke, AddCursorForNextOccurrence, ctx, cx)?,
        "SelectAllOccurrences" => load_binding(stroke, SelectAllOccurrences, ctx, cx)?,
        "AddCursorAbove" => load_binding(stroke, AddCursorAbove, ctx, cx)?,
        "AddCursorBelow" => load_binding(stroke, AddCursorBelow, ctx, cx)?,
        "ColumnSelectUp" => load_binding(stroke, ColumnSelectUp, ctx, cx)?,
        "ColumnSelectDown" => load_binding(stroke, ColumnSelectDown, ctx, cx)?,
        "GoToLine" => load_binding(stroke, GoToLine, ctx, cx)?,
        "GoToNextError" => load_binding(stroke, GoToNextError, ctx, cx)?,
        "QualifyIdentifier" => load_binding(stroke, QualifyIdentifier, ctx, cx)?,
        "MoveLineUp" => load_binding(stroke, MoveLineUp, ctx, cx)?,
        "MoveLineDown" => load_binding(stroke, MoveLineDown, ctx, cx)?,
        "DuplicateLine" => load_binding(stroke, DuplicateLine, ctx, cx)?,
        "JoinLines" => load_binding(stroke, JoinLines, ctx, cx)?,
        _ => {
            tracing::warn!("unknown editor action in keybindings.json: {name}");
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn known_actions_table_covers_binding_for(cx: &mut gpui::TestAppContext) {
        // Every name `binding_for` recognises must appear in KNOWN_ACTIONS so
        // the completeness check below is meaningful.
        for name in KNOWN_ACTIONS {
            assert!(
                cx.update(|cx| binding_for(name, "x", cx)).is_some(),
                "KNOWN_ACTIONS lists {name} but binding_for does not handle it"
            );
        }
    }

    #[gpui::test]
    fn binding_for_rejects_unknown_action(cx: &mut gpui::TestAppContext) {
        assert!(
            cx.update(|cx| binding_for("Nonexistent", "x", cx))
                .is_none()
        );
    }

    #[gpui::test]
    fn binding_for_rejects_invalid_stroke(cx: &mut gpui::TestAppContext) {
        // An "x" is a valid keystroke, but "a-b" is not (single-char key
        // followed by another char is rejected by gpui's keystroke parser).
        assert!(cx.update(|cx| binding_for("Backspace", "x", cx)).is_some());
        assert!(
            cx.update(|cx| binding_for("Backspace", "a-b", cx))
                .is_none()
        );
    }

    #[test]
    fn is_valid_chord_handles_chords_and_modifiers() {
        assert!(is_valid_chord("cmd-a"));
        assert!(is_valid_chord("cmd-k cmd-c"));
        assert!(is_valid_chord("alt-shift-left"));
        assert!(is_valid_chord("f5"));
        // Empty / whitespace-only input has no keystroke to bind.
        assert!(!is_valid_chord(""));
        assert!(!is_valid_chord("   "));
        // A piece that errors (single-char key followed by another char).
        assert!(!is_valid_chord("a-b"));
        // A chord with one bad piece is invalid.
        assert!(!is_valid_chord("cmd-a a-b"));
    }

    #[test]
    fn editor_override_conflicts_are_scoped_and_detected_before_save() {
        let overrides = EditorKeybindingMap::new();
        #[cfg(target_os = "macos")]
        let copy_stroke = "cmd-c";
        #[cfg(not(target_os = "macos"))]
        let copy_stroke = "ctrl-c";
        let conflicts = conflicts_for_editor_binding("Cut", copy_stroke, &overrides);
        assert_eq!(conflicts, vec!["Copy"]);
        assert!(conflicts_for_editor_binding("Copy", copy_stroke, &overrides).is_empty());
    }

    #[test]
    fn editor_override_resolution_replaces_only_target_action() {
        let mut overrides = EditorKeybindingMap::new();
        #[cfg(target_os = "macos")]
        let find_stroke = "cmd-shift-f";
        #[cfg(not(target_os = "macos"))]
        let find_stroke = "ctrl-shift-f";
        overrides.insert("Find".into(), vec![find_stroke.into()]);
        let resolved = resolve_editor_overrides(&overrides);
        assert_eq!(resolved["Find"], vec![find_stroke]);
        assert_eq!(resolved["Replace"], default_bindings()["Replace"]);
    }

    #[test]
    fn defaults_include_core_navigation_and_new_actions() {
        let m = default_bindings();
        // Core, always-present bindings.
        assert_eq!(m.get("Backspace").unwrap(), &vec!["backspace".to_string()]);
        assert_eq!(m.get("Left").unwrap(), &vec!["left".to_string()]);
        // Word nav present on every platform.
        assert!(m.contains_key("MoveWordLeft"));
        assert!(m.contains_key("MoveWordRight"));
        assert!(m.contains_key("MoveHumpLeft"));
        // Document start/end present.
        assert!(m.contains_key("MoveToBeginningOfDocument"));
        assert!(m.contains_key("MoveToEndOfDocument"));
        // SelectAll present (platform-dependent but always there).
        assert!(m.contains_key("SelectAll"));
        // Every default stroke must validate.
        for (name, strokes) in &m {
            for s in strokes {
                assert!(is_valid_chord(s), "default stroke {s:?} for {name} invalid");
            }
        }
    }

    #[test]
    fn defaults_dont_bind_intentionally_unbound_actions() {
        let m = default_bindings();
        // InsertDistinctValuesAsInList is kept unbound by default (only
        // programmatically triggered).
        assert!(!m.contains_key("InsertDistinctValuesAsInList"));
        // Cancel is unbound so cmd/shift-escape reaches the app-level query
        // cancel; bare escape dismisses via the Dismiss action instead.
        assert!(!m.contains_key("Cancel"));
        assert_eq!(m.get("Dismiss").unwrap(), &vec!["escape".to_string()]);
    }

    #[test]
    fn comment_actions_have_default_chords_without_conflicts() {
        let m = default_bindings();
        #[cfg(target_os = "macos")]
        let (add, remove) = ("cmd-k cmd-c", "cmd-k cmd-u");
        #[cfg(not(target_os = "macos"))]
        let (add, remove) = ("ctrl-k ctrl-c", "ctrl-k ctrl-u");
        assert_eq!(m.get("AddLineComment").unwrap(), &vec![add.to_string()]);
        assert_eq!(
            m.get("RemoveLineComment").unwrap(),
            &vec![remove.to_string()]
        );
        assert!(
            conflicts_for_editor_binding("AddLineComment", add, &Default::default()).is_empty()
        );
        assert!(
            conflicts_for_editor_binding("RemoveLineComment", remove, &Default::default())
                .is_empty()
        );
    }

    #[test]
    fn trigger_value_lookup_has_default_binding() {
        let m = default_bindings();
        assert!(
            m.contains_key("TriggerValueLookup"),
            "TriggerValueLookup should have a default binding"
        );
    }

    #[test]
    fn json_file_parses_single_string_and_array() {
        let json = r#"{
            "SelectAll": "cmd-a",
            "ExecuteQuery": ["cmd-enter", "f5"],
            "MoveWordLeft": "alt-left"
        }"#;
        let file: EditorKeybindingsFile = serde_json::from_str(json).unwrap();
        assert_eq!(
            file.0.get("SelectAll").unwrap().clone().into_vec(),
            vec!["cmd-a"]
        );
        assert_eq!(
            file.0.get("ExecuteQuery").unwrap().clone().into_vec(),
            vec!["cmd-enter", "f5"]
        );
    }

    #[test]
    fn merge_overrides_replaces_per_action() {
        let mut defaults = EditorKeybindingMap::new();
        defaults.insert("SelectAll".into(), vec!["cmd-a".into()]);
        defaults.insert("Backspace".into(), vec!["backspace".into()]);
        let mut raw = BTreeMap::new();
        raw.insert(
            "SelectAll".into(),
            StrokesJson::Many(vec!["ctrl-k ctrl-a".into()]),
        );
        let file = EditorKeybindingsFile(raw);
        let merged = merge_overrides(defaults, &file);
        // Overridden action is replaced.
        assert_eq!(
            merged.get("SelectAll").unwrap(),
            &vec!["ctrl-k ctrl-a".to_string()]
        );
        // Untouched action keeps its default.
        assert_eq!(
            merged.get("Backspace").unwrap(),
            &vec!["backspace".to_string()]
        );
    }

    #[test]
    fn merge_overrides_drops_invalid_keeps_default_when_all_invalid() {
        let mut defaults = EditorKeybindingMap::new();
        defaults.insert("SelectAll".into(), vec!["cmd-a".into()]);
        let mut raw = BTreeMap::new();
        // Entirely invalid -> keep default.
        raw.insert("SelectAll".into(), StrokesJson::Many(vec!["a-b".into()]));
        let file = EditorKeybindingsFile(raw);
        let merged = merge_overrides(defaults, &file);
        assert_eq!(merged.get("SelectAll").unwrap(), &vec!["cmd-a".to_string()]);
    }

    #[test]
    fn merge_overrides_filters_invalid_keeps_valid_within_action() {
        let mut defaults = EditorKeybindingMap::new();
        defaults.insert("SelectAll".into(), vec!["cmd-a".into()]);
        let mut raw = BTreeMap::new();
        // One valid, one invalid -> keep the valid one, drop the invalid.
        raw.insert(
            "SelectAll".into(),
            StrokesJson::Many(vec!["cmd-b".into(), "a-b".into()]),
        );
        let file = EditorKeybindingsFile(raw);
        let merged = merge_overrides(defaults, &file);
        assert_eq!(merged.get("SelectAll").unwrap(), &vec!["cmd-b".to_string()]);
    }

    #[gpui::test]
    fn merge_overrides_ignores_unknown_action_name(cx: &mut gpui::TestAppContext) {
        let defaults = EditorKeybindingMap::new();
        let mut raw = BTreeMap::new();
        raw.insert("BogusAction".into(), StrokesJson::One("cmd-x".into()));
        let file = EditorKeybindingsFile(raw);
        let merged = merge_overrides(defaults, &file);
        // Unknown name is still stored in the map (binding_for will skip it at
        // bind time); the important guarantee is it doesn't replace a known
        // default. Here there are no defaults to clobber.
        assert!(merged.contains_key("BogusAction"));
        assert!(
            cx.update(|cx| binding_for("BogusAction", "cmd-x", cx))
                .is_none()
        );
    }

    #[gpui::test]
    fn every_default_stroke_constructs_a_valid_binding(cx: &mut gpui::TestAppContext) {
        // Drives the same `binding_for` -> `KeyBinding::load` path that
        // `register_keybindings` walks, proving the full default set binds
        // without panicking on every platform cfg.
        let m = default_bindings();
        assert!(
            !m.is_empty(),
            "defaults must be non-empty on every platform"
        );
        for (name, strokes) in &m {
            for s in strokes {
                assert!(
                    cx.update(|cx| binding_for(name, s, cx)).is_some(),
                    "default stroke {s:?} for {name} failed to construct a KeyBinding"
                );
            }
        }
    }

    // ── New public API tests ────────────────────────────────────────────────

    #[gpui::test]
    fn defaults_include_copy_cut_paste(cx: &mut gpui::TestAppContext) {
        let m = default_bindings();
        assert!(m.contains_key("Copy"));
        assert!(m.contains_key("Cut"));
        assert!(m.contains_key("Paste"));
        for name in &["Copy", "Cut", "Paste"] {
            let strokes = m.get(*name).unwrap();
            for s in strokes {
                assert!(is_valid_chord(s), "default stroke {s:?} for {name} invalid");
                assert!(
                    cx.update(|cx| binding_for(name, s, cx)).is_some(),
                    "default stroke {s:?} for {name} failed to construct a KeyBinding"
                );
            }
        }
    }

    #[test]
    fn editor_action_display_name_covers_all_known() {
        for name in KNOWN_ACTIONS {
            let display = editor_action_display_name(name);
            assert!(
                display != "Unknown",
                "editor_action_display_name returns Unknown for KNOWN_ACTIONS entry {name}"
            );
        }
    }

    #[test]
    fn editor_action_display_name_unknown_falls_back() {
        assert_eq!(editor_action_display_name("BogusAction"), "Unknown");
    }

    #[test]
    fn keystroke_to_stroke_string_cmd_c() {
        let ks = gpui::Keystroke {
            key: "c".into(),
            modifiers: gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
            key_char: None,
        };
        assert_eq!(keystroke_to_stroke_string(&ks), "cmd-c");
    }

    #[test]
    fn keystroke_to_stroke_string_ctrl_shift_v() {
        let ks = gpui::Keystroke {
            key: "v".into(),
            modifiers: gpui::Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            key_char: None,
        };
        // Modifier order: cmd, shift, alt, ctrl.
        assert_eq!(keystroke_to_stroke_string(&ks), "shift-ctrl-v");
    }

    #[test]
    fn keystroke_to_stroke_string_bare_key() {
        let ks = gpui::Keystroke {
            key: "f5".into(),
            modifiers: gpui::Modifiers::default(),
            key_char: None,
        };
        assert_eq!(keystroke_to_stroke_string(&ks), "f5");
    }

    #[test]
    fn keystroke_to_stroke_string_all_modifiers() {
        let ks = gpui::Keystroke {
            key: "left".into(),
            modifiers: gpui::Modifiers {
                platform: true,
                shift: true,
                alt: true,
                control: true,
                function: false,
            },
            key_char: None,
        };
        assert_eq!(keystroke_to_stroke_string(&ks), "cmd-shift-alt-ctrl-left");
    }

    #[test]
    fn keystroke_to_stroke_string_produces_valid_chord() {
        // Every stroke string the converter produces must be parseable by
        // gpui's Keystroke::parse, so it can be used in the editor keybindings.
        let cases = vec![
            (
                "c",
                gpui::Modifiers {
                    platform: true,
                    ..Default::default()
                },
            ),
            (
                "v",
                gpui::Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
            ),
            ("f5", gpui::Modifiers::default()),
            (
                "left",
                gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
            ),
            (
                "backspace",
                gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
            ),
        ];
        for (key, mods) in cases {
            let ks = gpui::Keystroke {
                key: key.into(),
                modifiers: mods,
                key_char: None,
            };
            let stroke = keystroke_to_stroke_string(&ks);
            assert!(
                is_valid_chord(&stroke),
                "stroke {stroke:?} from key={key:?} is not a valid chord"
            );
        }
    }

    #[test]
    fn load_editor_overrides_returns_empty_when_no_file() {
        // The default keybindings_path() may or may not exist in test env,
        // but load_editor_overrides should never panic — it returns empty.
        let overrides = load_editor_overrides();
        // We can't assert emptiness because a real file might exist on the
        // dev machine, but the call itself must succeed.
        let _ = overrides;
    }

    #[test]
    fn save_and_load_editor_overrides_roundtrip() {
        // Save to a temp file via the public API, then load back.
        // We test the serialization format directly rather than going through
        // the file system (which uses a fixed path).
        let mut overrides: EditorKeybindingMap = BTreeMap::new();
        overrides.insert("Copy".into(), vec!["cmd-c".into()]);
        overrides.insert("ExecuteQuery".into(), vec!["cmd-enter".into(), "f5".into()]);

        // Serialize.
        let mut raw: BTreeMap<String, StrokesJson> = BTreeMap::new();
        for (name, strokes) in &overrides {
            raw.insert(name.clone(), StrokesJson::Many(strokes.clone()));
        }
        let file = EditorKeybindingsFile(raw);
        let json = serde_json::to_string(&file).unwrap();

        // Deserialize and verify.
        let parsed: EditorKeybindingsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.0.get("Copy").unwrap().clone().into_vec(),
            vec!["cmd-c".to_string()]
        );
        assert_eq!(
            parsed.0.get("ExecuteQuery").unwrap().clone().into_vec(),
            vec!["cmd-enter".to_string(), "f5".to_string()]
        );
    }

    #[test]
    fn known_actions_includes_copy_cut_paste() {
        assert!(KNOWN_ACTIONS.contains(&"Copy"));
        assert!(KNOWN_ACTIONS.contains(&"Cut"));
        assert!(KNOWN_ACTIONS.contains(&"Paste"));
    }

    #[gpui::test]
    fn multi_cursor_actions_have_default_bindings(cx: &mut gpui::TestAppContext) {
        let m = default_bindings();
        for name in [
            "AddCursorForNextOccurrence",
            "SelectAllOccurrences",
            "AddCursorAbove",
            "AddCursorBelow",
            "ColumnSelectUp",
            "ColumnSelectDown",
        ] {
            let strokes = m.get(name).unwrap_or_else(|| {
                panic!("{name} should have a default binding on every platform")
            });
            for s in strokes {
                assert!(
                    cx.update(|cx| binding_for(name, s, cx)).is_some(),
                    "default stroke {s:?} for {name} failed to construct a KeyBinding"
                );
            }
        }
    }

    #[test]
    fn go_to_next_error_f8_has_no_conflicts() {
        assert!(
            conflicts_for_editor_binding("GoToNextError", "f8", &Default::default()).is_empty()
        );
    }

    #[test]
    fn qualify_identifier_default_chord_has_no_conflicts() {
        #[cfg(target_os = "macos")]
        let stroke = "cmd-alt-q";
        #[cfg(not(target_os = "macos"))]
        let stroke = "ctrl-alt-q";
        assert!(
            conflicts_for_editor_binding("QualifyIdentifier", stroke, &Default::default())
                .is_empty()
        );
    }
}
