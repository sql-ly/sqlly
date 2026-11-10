//! Keybinding model â port of `ShortcutProfile.swift`, `KeybindingsEditorModel.swift`,
//! and `CommandRegistry.swift` keybinding data.
//!
//! This module is the pure, UI-free layer: keystroke/binding representation,
//! display strings, the three shortcut profiles (macOS Native / SSMS / Custom),
//! the resolver that merges defaults + SSMS overrides + custom overrides into
//! the active bindings, and the editor model with conflict detection.
//!
//! The GPUI `KeybindingsEditor` Entity (search + list + detail + buttons) is
//! layered on top of this pure model and lives further down in the same file.
//!
//! All pure logic is unit-tested (per AGENTS.md test mandate).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::command_palette::ShellCommand;
use gpui::{App, KeyBinding as GpuiKeyBinding, Keystroke};

// ââ Constants ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

/// Maximum number of distinct bindings a single command may carry.
pub const MAX_BINDINGS_PER_COMMAND: usize = 3;

/// Display label for the platform primary modifier (Cmd on macOS, Ctrl elsewhere).
#[cfg(target_os = "macos")]
pub const PRIMARY_LABEL: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
pub const PRIMARY_LABEL: &str = "Ctrl";

/// Display label for the option/alt modifier.
#[cfg(target_os = "macos")]
pub const OPTION_LABEL: &str = "Option";
#[cfg(not(target_os = "macos"))]
pub const OPTION_LABEL: &str = "Alt";

/// Unicode scalar for function key F`n` (1-based). F1 = 0xF704 â¦ F20 = 0xF717.
pub fn function_key_scalar(n: u32) -> char {
    char::from_u32(0xF704 + n - 1).unwrap()
}

const HOME_KEY: char = char::from_u32(0xF729).unwrap();
const END_KEY: char = char::from_u32(0xF72B).unwrap();
const PAGE_UP_KEY: char = char::from_u32(0xF72C).unwrap();
const PAGE_DOWN_KEY: char = char::from_u32(0xF72D).unwrap();
const LEFT_KEY: char = char::from_u32(0xF702).unwrap();
const RIGHT_KEY: char = char::from_u32(0xF703).unwrap();
const ESC: char = char::from_u32(0x1B).unwrap();
const BACKSPACE_KEY: char = '\u{8}';
const DELETE_KEY: char = char::from_u32(0xF728).unwrap();

// ââ Modifiers ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

/// The four macOS modifier flags, in display order (Cmd, Shift, Option, Ctrl).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub command: bool,
    pub shift: bool,
    pub option: bool,
    pub control: bool,
}

impl Modifiers {
    pub const fn none() -> Self {
        Self {
            command: false,
            shift: false,
            option: false,
            control: false,
        }
    }
    pub const fn command() -> Self {
        Self {
            command: true,
            ..Self::none()
        }
    }
    pub const fn control() -> Self {
        Self {
            control: true,
            ..Self::none()
        }
    }
    #[allow(dead_code)]
    pub const fn shift() -> Self {
        Self {
            shift: true,
            ..Self::none()
        }
    }
    pub const fn cmd_shift() -> Self {
        Self {
            command: true,
            shift: true,
            ..Self::none()
        }
    }
    #[allow(dead_code)]
    pub const fn cmd_option() -> Self {
        Self {
            command: true,
            option: true,
            ..Self::none()
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(self) -> bool {
        !self.command && !self.shift && !self.option && !self.control
    }

    /// Display pieces in macOS order: Cmd, Shift, Option, Ctrl.
    fn display_pieces(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.command {
            #[cfg(target_os = "macos")]
            v.push("Cmd");
            #[cfg(not(target_os = "macos"))]
            v.push("Ctrl");
        }
        if self.shift {
            v.push("Shift");
        }
        if self.option {
            #[cfg(target_os = "macos")]
            v.push("Option");
            #[cfg(not(target_os = "macos"))]
            v.push("Alt");
        }
        if self.control {
            v.push("Ctrl");
        }
        v
    }

    /// Descriptor pieces in lowercase: cmd, shift, option, ctrl.
    ///
    /// This is the stable, machine-readable persisted form and must be
    /// platform-independent so a file round-trips (and moves between OSes)
    /// without changing meaning: the logical `command` modifier always
    /// serialises as `cmd`, matching what `parse_descriptor_stroke` expects.
    /// Platform-specific labels (Ctrl on non-macOS) live in `display_pieces`.
    fn descriptor_pieces(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.command {
            v.push("cmd");
        }
        if self.shift {
            v.push("shift");
        }
        if self.option {
            v.push("alt");
        }
        if self.control {
            v.push("ctrl");
        }
        v
    }

    /// gpui keystroke tokens. Unlike `descriptor_pieces` (the canonical
    /// persisted form), the logical `command` modifier maps to the platform's
    /// actual gpui modifier â Cmd on macOS, Ctrl elsewhere â matching how
    /// `from_gpui_keystroke` reads events back. This is what `gpui_chord` feeds
    /// to `gpui::Keystroke::parse` when registering runtime bindings.
    fn gpui_pieces(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.command {
            #[cfg(target_os = "macos")]
            v.push("cmd");
            #[cfg(not(target_os = "macos"))]
            v.push("ctrl");
        }
        if self.shift {
            v.push("shift");
        }
        if self.option {
            v.push("alt");
        }
        if self.control {
            v.push("ctrl");
        }
        v
    }
}

// ââ KeyStroke ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

/// A single keystroke: a key plus optional modifiers. The key is a single
/// Unicode scalar â either a printable character (lowercased), a function-key
/// scalar (0xF704 + n â 1), or a navigation-key scalar (home/end/page-up/
/// page-down).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyStroke {
    pub key: char,
    pub modifiers: Modifiers,
}

impl KeyStroke {
    pub fn new(key: char, modifiers: Modifiers) -> Self {
        // Normalise to lowercase, matching Swift's `from(characters:)`.
        let key = key.to_lowercase().next().unwrap_or(key);
        Self { key, modifiers }
    }

    pub fn function_key(n: u32) -> Self {
        Self {
            key: function_key_scalar(n),
            modifiers: Modifiers::none(),
        }
    }

    #[allow(dead_code)]
    pub fn home_key() -> Self {
        Self {
            key: HOME_KEY,
            modifiers: Modifiers::none(),
        }
    }
    #[allow(dead_code)]
    pub fn end_key() -> Self {
        Self {
            key: END_KEY,
            modifiers: Modifiers::none(),
        }
    }
    #[allow(dead_code)]
    pub fn page_up_key() -> Self {
        Self {
            key: PAGE_UP_KEY,
            modifiers: Modifiers::none(),
        }
    }
    #[allow(dead_code)]
    pub fn page_down_key() -> Self {
        Self {
            key: PAGE_DOWN_KEY,
            modifiers: Modifiers::none(),
        }
    }

    /// Human-readable label, e.g. "Cmd+Shift+P", "F5", "fn+â", "Cmd+Enter".
    pub fn display_string(&self) -> String {
        let mut pieces = self.modifiers.display_pieces();
        pieces.push(display_key(self.key));
        pieces.join("+")
    }

    /// Stable, machine-readable descriptor, e.g. "cmd+shift+p", "f5", "fn+left".
    /// Canonical and platform-independent: the logical `command` modifier always
    /// serialises as `cmd` so persisted files round-trip and move between OSes.
    pub fn descriptor_string(&self) -> String {
        let mut pieces = self.modifiers.descriptor_pieces();
        pieces.push(descriptor_key(self.key));
        pieces.join("+")
    }

    /// Platform-specific descriptor for gpui registration (`command` â Cmd on
    /// macOS, Ctrl elsewhere). Distinct from `descriptor_string`, which is the
    /// canonical persisted form.
    fn gpui_descriptor_string(&self) -> String {
        let mut pieces = self.modifiers.gpui_pieces();
        pieces.push(descriptor_key(self.key));
        pieces.join("+")
    }
}

fn display_key(key: char) -> &'static str {
    // Static strings for common cases; falls back to a formatted String for
    // the general case (boxed via Box::leak for 'static lifetime â these are
    // a tiny fixed set of chars, not unbounded).
    match key as u32 {
        v if (0xF704..=0xF717).contains(&v) => {
            // F-keys are dynamic; leak is fine (â¤20 entries).
            Box::leak(format!("F{}", v - 0xF704 + 1).into_boxed_str())
        }
        _ => match key {
            #[cfg(target_os = "macos")]
            HOME_KEY => "fn+â",
            #[cfg(not(target_os = "macos"))]
            HOME_KEY => "Home",
            #[cfg(target_os = "macos")]
            END_KEY => "fn+â",
            #[cfg(not(target_os = "macos"))]
            END_KEY => "End",
            #[cfg(target_os = "macos")]
            PAGE_UP_KEY => "fn+â",
            #[cfg(not(target_os = "macos"))]
            PAGE_UP_KEY => "Page Up",
            #[cfg(target_os = "macos")]
            PAGE_DOWN_KEY => "fn+â",
            #[cfg(not(target_os = "macos"))]
            PAGE_DOWN_KEY => "Page Down",
            LEFT_KEY => "â",
            RIGHT_KEY => "â",
            ' ' => "Space",
            '\r' => "Enter",
            '\t' => "Tab",
            BACKSPACE_KEY => "Backspace",
            DELETE_KEY => "Delete",
            ESC => "Esc",
            _ => Box::leak(key.to_uppercase().collect::<String>().into_boxed_str()),
        },
    }
}

fn descriptor_key(key: char) -> &'static str {
    match key as u32 {
        v if (0xF704..=0xF717).contains(&v) => {
            Box::leak(format!("f{}", v - 0xF704 + 1).into_boxed_str())
        }
        _ => match key {
            #[cfg(target_os = "macos")]
            HOME_KEY => "fn+left",
            #[cfg(not(target_os = "macos"))]
            HOME_KEY => "home",
            #[cfg(target_os = "macos")]
            END_KEY => "fn+right",
            #[cfg(not(target_os = "macos"))]
            END_KEY => "end",
            #[cfg(target_os = "macos")]
            PAGE_UP_KEY => "fn+up",
            #[cfg(not(target_os = "macos"))]
            PAGE_UP_KEY => "pageup",
            #[cfg(target_os = "macos")]
            PAGE_DOWN_KEY => "fn+down",
            #[cfg(not(target_os = "macos"))]
            PAGE_DOWN_KEY => "pagedown",
            LEFT_KEY => "left",
            RIGHT_KEY => "right",
            ' ' => "space",
            '\r' => "enter",
            '\t' => "tab",
            BACKSPACE_KEY => "backspace",
            DELETE_KEY => "delete",
            '/' => "/",
            '.' => ".",
            ',' => ",",
            ESC => "escape",
            _ => Box::leak(key.to_lowercase().collect::<String>().into_boxed_str()),
        },
    }
}

// ââ KeyBinding âââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

/// One or two keystrokes (a chord). Single-stroke bindings are the common case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub strokes: Vec<KeyStroke>,
    /// Optional gpui key-context predicate (e.g. `SqlEditor` or
    /// `!SqlEditor`) restricting when the binding fires. `None` = global.
    pub when: Option<String>,
}

impl KeyBinding {
    pub fn new(key: char, modifiers: Modifiers) -> Self {
        Self {
            strokes: vec![KeyStroke::new(key, modifiers)],
            when: None,
        }
    }

    pub fn single(stroke: KeyStroke) -> Self {
        Self {
            strokes: vec![stroke],
            when: None,
        }
    }

    #[allow(dead_code)]
    pub fn is_single_stroke(&self) -> bool {
        self.strokes.len() == 1
    }

    pub fn display_string(&self) -> String {
        self.strokes
            .iter()
            .map(|s| s.display_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn descriptor_string(&self) -> String {
        self.strokes
            .iter()
            .map(|s| s.descriptor_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Platform-specific descriptor used to build the gpui chord (see
    /// [`KeyStroke::gpui_descriptor_string`]).
    fn gpui_descriptor_string(&self) -> String {
        self.strokes
            .iter()
            .map(|s| s.gpui_descriptor_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ââ GPUI Keystroke conversion ââââââââââââââââââââââââââââââââââââââââââââââ

/// Convert a `gpui::Keystroke` (from a `KeyDownEvent`) into the app's
/// `KeyStroke` representation. Returns `None` for keys that can't be
/// represented (e.g. bare modifier presses).
pub fn from_gpui_keystroke(ks: &gpui::Keystroke) -> Option<KeyStroke> {
    #[cfg(target_os = "macos")]
    let mods = Modifiers {
        command: ks.modifiers.platform,
        shift: ks.modifiers.shift,
        option: ks.modifiers.alt,
        control: ks.modifiers.control,
    };
    #[cfg(not(target_os = "macos"))]
    let mods = Modifiers {
        command: ks.modifiers.control,
        shift: ks.modifiers.shift,
        option: ks.modifiers.alt,
        control: false,
    };
    let key_char = gpui_key_to_char(&ks.key)?;
    Some(KeyStroke::new(key_char, mods))
}

/// Map a gpui key string (e.g. "a", "enter", "f5", "home") to the app's
/// char-based key representation.
fn gpui_key_to_char(key: &str) -> Option<char> {
    match key {
        "enter" | "return" => Some('\r'),
        "backspace" => Some(BACKSPACE_KEY),
        "delete" => Some(DELETE_KEY),
        "escape" => Some(ESC),
        "tab" => Some('\t'),
        "space" => Some(' '),
        "home" => Some(HOME_KEY),
        "end" => Some(END_KEY),
        "pageup" => Some(PAGE_UP_KEY),
        "pagedown" => Some(PAGE_DOWN_KEY),
        "left" => Some(LEFT_KEY),
        "right" => Some(RIGHT_KEY),
        s if s.starts_with('f') && s[1..].parse::<u32>().is_ok() && {
            let n = s[1..].parse::<u32>().unwrap();
            (1..=20).contains(&n)
        } =>
        {
            let n = s[1..].parse::<u32>().unwrap();
            Some(function_key_scalar(n))
        }
        s if s.chars().count() == 1 => s.chars().next(),
        _ => None,
    }
}

// ââ ShortcutProfile ââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

/// The active shortcut profile. `MacOSNative` is the default; `Ssms` layers
/// SSMS-style overrides on top of defaults; `Custom` is purely user-driven.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Deserialize, serde::Serialize,
)]
pub enum ShortcutProfile {
    #[serde(rename = "platformNative", alias = "macOSNative")]
    #[default]
    MacOSNative,
    #[serde(rename = "ssms")]
    Ssms,
    #[serde(rename = "custom")]
    Custom,
}

impl ShortcutProfile {
    pub fn label(self) -> String {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacOSNative => {
                crate::localization::text("settings.keybindings.profile.macos_native")
            }
            #[cfg(not(target_os = "macos"))]
            Self::MacOSNative => {
                crate::localization::text("settings.keybindings.profile.platform_native")
            }
            Self::Ssms => "SSMS".to_string(),
            Self::Custom => crate::localization::text("settings.keybindings.profile.custom"),
        }
    }
}

// ââ Default bindings + SSMS overrides ââââââââââââââââââââââââââââââââââââââ

/// The macOS-native default bindings, keyed by command id. Mirrors the
/// `cx.bind_keys([...])` registrations in `main.rs` and `editor::register_keybindings`.
pub fn default_bindings() -> HashMap<String, Vec<KeyBinding>> {
    let mut m = HashMap::new();
    m.insert(
        "file.newQuery".into(),
        vec![KeyBinding::new('n', Modifiers::command())],
    );
    m.insert(
        "file.closeTab".into(),
        vec![KeyBinding::new('w', Modifiers::command())],
    );
    m.insert("file.reopenClosedTab".into(), {
        let mut modifiers = Modifiers::command();
        modifiers.shift = true;
        vec![KeyBinding::new('t', modifiers)]
    });
    m.insert(
        "query.execute".into(),
        vec![
            KeyBinding::new('\r', Modifiers::command()),
            KeyBinding::single(KeyStroke::function_key(5)),
        ],
    );
    m.insert(
        "query.executeKeepResults".into(),
        vec![KeyBinding::new('\\', Modifiers::command())],
    );
    #[cfg(target_os = "macos")]
    m.insert(
        "query.cancel".into(),
        vec![KeyBinding::new(ESC, Modifiers::command())],
    );
    #[cfg(not(target_os = "macos"))]
    m.insert(
        "query.cancel".into(),
        vec![KeyBinding::new(ESC, Modifiers::shift())],
    );
    m.insert(
        "query.runPicker".into(),
        vec![KeyBinding::new('\r', {
            let mut modifiers = Modifiers::command();
            modifiers.shift = true;
            modifiers
        })],
    );
    m.insert(
        "query.validate".into(),
        vec![KeyBinding::new('v', Modifiers::cmd_shift())],
    );
    // ââ¥â§F on macOS; the command modifier maps to Ctrl elsewhere, giving
    // Ctrl+Alt+Shift+F on Windows/Linux.
    m.insert("editor.minify".into(), {
        let mut modifiers = Modifiers::cmd_shift();
        modifiers.option = true;
        vec![KeyBinding::new('f', modifiers)]
    });
    m.insert(
        "transaction.commit".into(),
        vec![KeyBinding::new('\r', Modifiers::cmd_option())],
    );
    m.insert(
        "transaction.rollback".into(),
        vec![KeyBinding::new(BACKSPACE_KEY, Modifiers::cmd_option())],
    );
    m.insert(
        "transaction.toggleMode".into(),
        vec![KeyBinding::new('t', Modifiers::cmd_option())],
    );
    m.insert(
        "view.toggleSidebar".into(),
        vec![KeyBinding::new('b', Modifiers::command())],
    );
    m.insert(
        "view.toggleResultsPane".into(),
        vec![KeyBinding::new('j', Modifiers::command())],
    );
    m.insert(
        "view.toggleResultsMaximized".into(),
        vec![KeyBinding::new('j', Modifiers::cmd_shift())],
    );
    #[cfg(not(target_arch = "wasm32"))]
    m.insert(
        "view.detachResults".into(),
        vec![KeyBinding::new('j', Modifiers::cmd_option())],
    );
    #[cfg(not(target_arch = "wasm32"))]
    m.insert("view.reattachResults".into(), Vec::new());
    m.insert("tools.queryBuilder".into(), Vec::new());
    m.insert(
        "view.selectOutputResults".into(),
        vec![KeyBinding::new('1', Modifiers::command())],
    );
    m.insert(
        "view.selectOutputMessages".into(),
        vec![KeyBinding::new('2', Modifiers::command())],
    );
    m.insert(
        "view.selectOutputPlan".into(),
        vec![KeyBinding::new('3', Modifiers::command())],
    );
    m.insert(
        "view.selectOutputHistory".into(),
        vec![KeyBinding::new('4', Modifiers::command())],
    );
    m.insert(
        "view.selectOutputDdl".into(),
        vec![KeyBinding::new('5', Modifiers::command())],
    );
    m.insert(
        "view.selectOutputDiff".into(),
        vec![KeyBinding::new('6', Modifiers::command())],
    );
    for n in 1u8..=9 {
        let mut modifiers = Modifiers::control();
        modifiers.shift = true;
        m.insert(
            format!("connection.quickSwitch{n}"),
            vec![KeyBinding::new(char::from(b'0' + n), modifiers)],
        );
    }
    m.insert(
        "view.toggleHistoryPanel".into(),
        vec![KeyBinding::new('h', Modifiers::cmd_option())],
    );
    m.insert(
        "view.toggleAiPanel".into(),
        vec![KeyBinding::new('i', Modifiers::cmd_shift())],
    );
    // Cmd+Shift+I already toggles the AI panel, so Cell Details ("I" for
    // inspect) takes ⌘⌥I / Ctrl+Alt+I, free in both the app and editor maps.
    m.insert(
        "view.toggleCellDetails".into(),
        vec![KeyBinding::new('i', Modifiers::cmd_option())],
    );
    m.insert(
        "query.sendSelectionToAi".into(),
        vec![KeyBinding::new('a', Modifiers::cmd_shift())],
    );
    m.insert("ai.toggleAgentMode".into(), Vec::new());
    m.insert(
        "view.toggleLogConsole".into(),
        vec![KeyBinding::new('l', Modifiers::cmd_shift())],
    );
    m.insert(
        "tools.commandPalette".into(),
        vec![KeyBinding::new('p', Modifiers::cmd_shift())],
    );
    m.insert(
        "tools.quickOpen".into(),
        vec![KeyBinding::new('p', Modifiers::command())],
    );
    // Send Feedback: ⌘⌥F on macOS, Ctrl+Alt+F elsewhere (F for feedback;
    // ⌘⌥⇧F is Minify, and plain ⌘⇧F is taken).
    m.insert(
        "help.sendFeedback".into(),
        vec![KeyBinding::new('f', Modifiers::cmd_option())],
    );
    m.insert(
        "window.nextEditorTab".into(),
        vec![
            KeyBinding::new(
                '\t',
                Modifiers {
                    control: true,
                    ..Modifiers::none()
                },
            ),
            #[cfg(target_os = "macos")]
            KeyBinding::new(']', Modifiers::cmd_shift()),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::single(KeyStroke::new(PAGE_DOWN_KEY, Modifiers::command())),
        ],
    );
    m.insert(
        "window.previousEditorTab".into(),
        vec![
            KeyBinding::new(
                '\t',
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::none()
                },
            ),
            #[cfg(target_os = "macos")]
            KeyBinding::new('[', Modifiers::cmd_shift()),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::single(KeyStroke::new(PAGE_UP_KEY, Modifiers::command())),
        ],
    );
    m.insert(
        "window.previousTabInHistory".into(),
        vec![KeyBinding::new(
            LEFT_KEY,
            Modifiers {
                control: true,
                option: true,
                ..Modifiers::none()
            },
        )],
    );
    m.insert(
        "window.nextTabInHistory".into(),
        vec![KeyBinding::new(
            RIGHT_KEY,
            Modifiers {
                control: true,
                option: true,
                ..Modifiers::none()
            },
        )],
    );
    // Empty defaults so a user override for a configured slot still resolves
    // through `runtime_binding_specs` without shipping a default chord.
    for slot in 1u8..=9 {
        m.insert(format!("quickAction.slot{slot}"), Vec::new());
    }
    let results_pane = |stroke: KeyStroke| KeyBinding {
        strokes: vec![stroke],
        when: Some("ResultsPane".to_string()),
    };
    m.insert(
        "results.findInGrid".into(),
        vec![results_pane(KeyStroke::new('f', Modifiers::command()))],
    );
    m.insert(
        "results.gotoColumn".into(),
        vec![results_pane(KeyStroke::new('g', Modifiers::command()))],
    );
    m.insert(
        "results.stageInsertRow".into(),
        vec![results_pane(KeyStroke::new('n', Modifiers::command()))],
    );
    m.insert(
        "results.stageDeleteRow".into(),
        vec![
            results_pane(KeyStroke {
                key: DELETE_KEY,
                modifiers: Modifiers::none(),
            }),
            results_pane(KeyStroke::new(BACKSPACE_KEY, Modifiers::none())),
        ],
    );
    m.insert(
        "results.stage_undo".into(),
        vec![results_pane(KeyStroke::new('z', Modifiers::command()))],
    );
    m.insert(
        "results.stage_redo".into(),
        vec![results_pane(KeyStroke::new('z', Modifiers::cmd_shift()))],
    );
    m.insert(
        "results.refresh".into(),
        vec![results_pane(KeyStroke::new('r', Modifiers::command()))],
    );
    // F6 jumps keyboard focus between the SQL editor and the result grid, so
    // it has to fire from both key contexts.
    let editor_pane = |stroke: KeyStroke| KeyBinding {
        strokes: vec![stroke],
        when: Some("SqlEditor".to_string()),
    };
    m.insert(
        "results.focusToggle".into(),
        vec![
            editor_pane(KeyStroke::function_key(6)),
            results_pane(KeyStroke::function_key(6)),
        ],
    );
    m
}

/// SSMS-profile overrides layered on top of defaults. Keys here replace the
/// default bindings for that command when the SSMS profile is active.
pub fn ssms_overrides() -> HashMap<String, KeyBinding> {
    let mut m = HashMap::new();
    m.insert(
        "query.execute".into(),
        KeyBinding::single(KeyStroke::function_key(5)),
    );
    // file.newQuery stays âN (same as default).
    m
}

// ââ Resolver âââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

/// Resolves the active bindings for a command given the profile and custom
/// overrides.
///
/// Resolution order (mirrors Swift `ShortcutResolver.bindings(for:)`):
/// 1. If custom overrides exist for this command (non-empty), use them (capped
///    at `MAX_BINDINGS_PER_COMMAND`).
/// 2. Else if the SSMS profile is active and an SSMS override exists, use it.
/// 3. Else fall back to the default bindings.
pub fn resolve_bindings(
    command_id: &str,
    profile: ShortcutProfile,
    overrides: &HashMap<String, Vec<KeyBinding>>,
    defaults: &HashMap<String, Vec<KeyBinding>>,
    ssms: &HashMap<String, KeyBinding>,
) -> Vec<KeyBinding> {
    if let Some(custom) = overrides.get(command_id)
        && !custom.is_empty()
    {
        return custom
            .iter()
            .take(MAX_BINDINGS_PER_COMMAND)
            .cloned()
            .collect();
    }
    if profile == ShortcutProfile::Ssms
        && let Some(override_binding) = ssms.get(command_id)
    {
        return vec![override_binding.clone()];
    }
    defaults.get(command_id).cloned().unwrap_or_default()
}

/// Resolve every command covered by the app-level keybinding model. Sorting
/// happens at the registration boundary so duplicate bindings have stable,
/// testable precedence instead of depending on `HashMap` iteration order.
pub fn resolved_app_bindings(settings: &KeybindingSettings) -> HashMap<String, Vec<KeyBinding>> {
    let defaults = default_bindings();
    let ssms = ssms_overrides();
    defaults
        .keys()
        .map(|command_id| {
            (
                command_id.clone(),
                resolve_bindings(
                    command_id,
                    settings.profile,
                    &settings.overrides,
                    &defaults,
                    &ssms,
                ),
            )
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub binding: KeyBinding,
    pub command_ids: Vec<String>,
}

pub fn detect_conflicts(bindings: &HashMap<String, Vec<KeyBinding>>) -> Vec<KeybindingConflict> {
    let mut owners: HashMap<KeyBinding, Vec<String>> = HashMap::new();
    for (command_id, command_bindings) in bindings {
        for binding in command_bindings {
            owners
                .entry(binding.clone())
                .or_default()
                .push(command_id.clone());
        }
    }
    let mut conflicts: Vec<_> = owners
        .into_iter()
        .filter_map(|(binding, mut command_ids)| {
            command_ids.sort();
            command_ids.dedup();
            (command_ids.len() > 1).then_some(KeybindingConflict {
                binding,
                command_ids,
            })
        })
        .collect();
    conflicts.sort_by(|left, right| {
        left.binding
            .descriptor_string()
            .cmp(&right.binding.descriptor_string())
    });
    conflicts
}

fn gpui_chord(binding: &KeyBinding) -> Option<String> {
    // The gpui chord is platform-specific (command â Cmd on macOS, Ctrl
    // elsewhere), unlike the canonical persisted descriptor.
    let chord = binding.gpui_descriptor_string().replace('+', "-");
    let valid = chord
        .split_whitespace()
        .all(|stroke| parse_keystroke(stroke).is_ok());
    valid.then_some(chord)
}

/// Pure startup-registration plan used by both the real startup path and its
/// tests. Invalid persisted strokes have already been rejected on load; this
/// final GPUI parse check keeps registration panic-free if a model is built
/// programmatically.
/// Validate a `when` expression against gpui's key-context predicate
/// grammar; invalid expressions are dropped (with a warning) rather than
/// silently disabling the binding.
fn validate_when_clause(source: &str) -> Option<String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    match gpui::KeyBindingContextPredicate::parse(trimmed) {
        Ok(_) => Some(trimmed.to_string()),
        Err(error) => {
            tracing::warn!("keybindings.json: invalid when clause {trimmed:?}: {error}");
            None
        }
    }
}

/// The host OS whose keystroke conventions runtime strings are parsed under.
/// gpui's `Keystroke::parse` only takes static literals now; strings built at
/// runtime (settings, keybindings.json) go through `parse_with_os`.
pub(crate) const fn host_os() -> gpui::OperatingSystem {
    if cfg!(target_os = "macos") {
        gpui::OperatingSystem::Mac
    } else if cfg!(target_os = "windows") {
        gpui::OperatingSystem::Windows
    } else {
        gpui::OperatingSystem::Linux
    }
}

/// Whether the platform's primary shortcut modifier is held: ⌘ on macOS,
/// Ctrl elsewhere — including the browser build, which folds a Mac's ⌘ into
/// Ctrl before gpui sees the event (see `mac_command_key` in apps/browser).
/// gpui's `Modifiers::secondary(cx)` asks the host OS at runtime instead, so
/// on a Mac browser it would wait for a ⌘ that never arrives.
pub(crate) fn primary_modifier(modifiers: &gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}

/// Parse a keystroke string built at runtime.
pub(crate) fn parse_keystroke(source: &str) -> Result<Keystroke, gpui::InvalidKeystrokeError> {
    Keystroke::parse_with_os(source, host_os())
}

/// Build a runtime binding for `stroke` (already validated by the caller).
/// `KeyBinding::new` only takes static literals, so runtime strokes load
/// through `KeyBinding::load`; a stroke or `when` clause that still fails to
/// parse is logged and skipped instead of panicking.
pub(crate) fn load_binding<A: gpui::Action>(
    stroke: &str,
    action: A,
    when: Option<&str>,
    cx: &App,
) -> Option<GpuiKeyBinding> {
    let predicate = match when {
        Some(when) => match gpui::KeyBindingContextPredicate::parse(when) {
            Ok(predicate) => Some(std::rc::Rc::new(predicate)),
            Err(error) => {
                tracing::warn!(
                    "skipping binding {stroke:?}: invalid when clause {when:?}: {error}"
                );
                return None;
            }
        },
        None => None,
    };
    match GpuiKeyBinding::load(stroke, Box::new(action), predicate, false, None, cx) {
        Ok(binding) => Some(binding),
        Err(error) => {
            tracing::warn!("skipping invalid keystroke {stroke:?}: {error}");
            None
        }
    }
}

pub fn runtime_binding_specs(
    settings: &KeybindingSettings,
) -> Vec<(String, String, Option<String>)> {
    let resolved = resolved_app_bindings(settings);
    let mut command_ids: Vec<_> = resolved.keys().cloned().collect();
    command_ids.sort();
    command_ids
        .into_iter()
        .flat_map(|command_id| {
            resolved[&command_id]
                .iter()
                .filter_map(|binding| {
                    gpui_chord(binding)
                        .map(|chord| (command_id.clone(), chord, binding.when.clone()))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn register_app_keybindings(cx: &mut App) {
    let settings = load_keybinding_settings(&keybindings_path());
    set_active_settings(Some(settings.clone()));
    register_app_keybindings_from(&settings, cx);
}

pub fn register_app_keybindings_from(settings: &KeybindingSettings, cx: &mut App) {
    for (command_id, stroke, when) in runtime_binding_specs(settings) {
        let ctx: Option<&str> = when.as_deref();
        let binding = match command_id.as_str() {
            "file.newQuery" => load_binding(&stroke, crate::app::NewQueryTab, ctx, cx),
            "file.closeTab" => load_binding(&stroke, crate::app::CloseTab, ctx, cx),
            "file.reopenClosedTab" => load_binding(&stroke, crate::app::ReopenClosedTab, ctx, cx),
            "query.execute" => load_binding(&stroke, crate::app::ExecuteActiveQuery, ctx, cx),
            "query.executeKeepResults" => {
                load_binding(&stroke, crate::app::ExecuteActiveQueryKeepResults, ctx, cx)
            }
            "results.findInGrid" => load_binding(&stroke, crate::app::FindInResults, ctx, cx),
            "results.gotoColumn" => load_binding(&stroke, crate::app::GotoColumnInResults, ctx, cx),
            "results.focusToggle" => {
                load_binding(&stroke, crate::app::ToggleEditorResultsFocus, ctx, cx)
            }
            "results.stageInsertRow" => {
                load_binding(&stroke, crate::app::StageInsertRowInResults, ctx, cx)
            }
            "results.stageDeleteRow" => {
                load_binding(&stroke, crate::app::StageDeleteRowInResults, ctx, cx)
            }
            "results.stage_undo" => load_binding(&stroke, crate::app::StageUndoInResults, ctx, cx),
            "results.stage_redo" => load_binding(&stroke, crate::app::StageRedoInResults, ctx, cx),
            "results.refresh" => load_binding(&stroke, crate::app::RefreshResults, ctx, cx),
            "query.runPicker" => load_binding(&stroke, crate::app::RunPicker, ctx, cx),
            "query.runNotebookCell" => load_binding(&stroke, crate::app::RunNotebookCell, ctx, cx),
            "query.runAllNotebookCells" => {
                load_binding(&stroke, crate::app::RunAllNotebookCells, ctx, cx)
            }
            "query.cancel" => load_binding(&stroke, crate::app::CancelActiveQuery, ctx, cx),
            "transaction.commit" => load_binding(&stroke, crate::app::CommitTransaction, ctx, cx),
            "transaction.rollback" => {
                load_binding(&stroke, crate::app::RollbackTransaction, ctx, cx)
            }
            "transaction.toggleMode" => {
                load_binding(&stroke, crate::app::ToggleTransactionMode, ctx, cx)
            }
            "query.runOnTargets" => load_binding(&stroke, crate::app::RunOnTargets, ctx, cx),
            "query.validate" => load_binding(&stroke, crate::app::ValidateActiveQuery, ctx, cx),
            "editor.minify" => load_binding(&stroke, crate::app::MinifyActiveQuery, ctx, cx),
            "view.toggleSidebar" => load_binding(&stroke, crate::app::ToggleSidebar, ctx, cx),
            "view.toggleResultsPane" => {
                load_binding(&stroke, crate::app::ToggleResultsPane, ctx, cx)
            }
            "view.toggleResultsMaximized" => {
                load_binding(&stroke, crate::app::ToggleResultsMaximized, ctx, cx)
            }
            "view.detachResults" => load_binding(&stroke, crate::app::DetachResults, ctx, cx),
            "view.reattachResults" => load_binding(&stroke, crate::app::ReattachResults, ctx, cx),
            "view.toggleHistoryPanel" => {
                load_binding(&stroke, crate::app::ToggleHistoryPanel, ctx, cx)
            }
            "view.toggleAiPanel" => load_binding(&stroke, crate::app::ToggleAiPanel, ctx, cx),
            "view.toggleCellDetails" => {
                load_binding(&stroke, crate::app::ToggleCellDetails, ctx, cx)
            }
            "query.sendSelectionToAi" => {
                load_binding(&stroke, crate::app::SendSelectionToAi, ctx, cx)
            }
            "ai.toggleAgentMode" => load_binding(&stroke, crate::app::ToggleAgentMode, ctx, cx),
            "view.toggleLogConsole" => load_binding(&stroke, crate::app::ToggleLogConsole, ctx, cx),
            "tools.commandPalette" => {
                load_binding(&stroke, crate::app::ToggleCommandPalette, ctx, cx)
            }
            "tools.queryBuilder" => load_binding(&stroke, crate::app::QueryBuilder, ctx, cx),
            "tools.quickOpen" => load_binding(&stroke, crate::app::QuickOpen, ctx, cx),
            "help.sendFeedback" => load_binding(&stroke, crate::app::SendFeedback, ctx, cx),
            "window.nextEditorTab" => load_binding(&stroke, crate::app::NextEditorTab, ctx, cx),
            "window.previousEditorTab" => {
                load_binding(&stroke, crate::app::PreviousEditorTab, ctx, cx)
            }
            "window.previousTabInHistory" => {
                load_binding(&stroke, crate::app::PreviousTabInHistory, ctx, cx)
            }
            "window.nextTabInHistory" => {
                load_binding(&stroke, crate::app::NextTabInHistory, ctx, cx)
            }
            "quickAction.slot1" => load_binding(&stroke, crate::app::RunQuickActionSlot1, ctx, cx),
            "quickAction.slot2" => load_binding(&stroke, crate::app::RunQuickActionSlot2, ctx, cx),
            "quickAction.slot3" => load_binding(&stroke, crate::app::RunQuickActionSlot3, ctx, cx),
            "quickAction.slot4" => load_binding(&stroke, crate::app::RunQuickActionSlot4, ctx, cx),
            "quickAction.slot5" => load_binding(&stroke, crate::app::RunQuickActionSlot5, ctx, cx),
            "quickAction.slot6" => load_binding(&stroke, crate::app::RunQuickActionSlot6, ctx, cx),
            "quickAction.slot7" => load_binding(&stroke, crate::app::RunQuickActionSlot7, ctx, cx),
            "quickAction.slot8" => load_binding(&stroke, crate::app::RunQuickActionSlot8, ctx, cx),
            "quickAction.slot9" => load_binding(&stroke, crate::app::RunQuickActionSlot9, ctx, cx),
            "connection.quickSwitch1" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection1, ctx, cx)
            }
            "connection.quickSwitch2" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection2, ctx, cx)
            }
            "connection.quickSwitch3" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection3, ctx, cx)
            }
            "connection.quickSwitch4" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection4, ctx, cx)
            }
            "connection.quickSwitch5" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection5, ctx, cx)
            }
            "connection.quickSwitch6" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection6, ctx, cx)
            }
            "connection.quickSwitch7" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection7, ctx, cx)
            }
            "connection.quickSwitch8" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection8, ctx, cx)
            }
            "connection.quickSwitch9" => {
                load_binding(&stroke, crate::app::QuickSwitchConnection9, ctx, cx)
            }
            _ => continue,
        };
        let Some(binding) = binding else { continue };
        cx.bind_keys([binding]);
    }
}

#[allow(dead_code)]
/// Convenience: the first active binding (or `None`).
pub fn binding_for(
    command_id: &str,
    profile: ShortcutProfile,
    overrides: &HashMap<String, Vec<KeyBinding>>,
    defaults: &HashMap<String, Vec<KeyBinding>>,
    ssms: &HashMap<String, KeyBinding>,
) -> Option<KeyBinding> {
    resolve_bindings(command_id, profile, overrides, defaults, ssms)
        .into_iter()
        .next()
}

/// Convenience: the display string for all active bindings joined by ", ".
pub fn display_string_for(
    command_id: &str,
    profile: ShortcutProfile,
    overrides: &HashMap<String, Vec<KeyBinding>>,
    defaults: &HashMap<String, Vec<KeyBinding>>,
    ssms: &HashMap<String, KeyBinding>,
) -> String {
    resolve_bindings(command_id, profile, overrides, defaults, ssms)
        .iter()
        .map(|b| b.display_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Display hint for a command's active binding(s) against caller-supplied
/// overrides, under the macOS-native profile (the only profile the runtime
/// keymap currently registers). Empty when the command carries no binding.
#[cfg(test)]
fn hint_for(command_id: &str, overrides: &KeyBindingOverrides) -> String {
    hint_for_settings(
        command_id,
        &KeybindingSettings {
            profile: ShortcutProfile::MacOSNative,
            overrides: overrides.clone(),
        },
    )
}

pub fn hint_for_settings(command_id: &str, settings: &KeybindingSettings) -> String {
    display_string_for(
        command_id,
        settings.profile,
        &settings.overrides,
        default_bindings_table(),
        ssms_overrides_table(),
    )
}

/// The default binding table, built once. `default_bindings()` allocates a
/// fresh map on every call, and hint lookups run on render paths (tab bar,
/// output-tab tooltips) at frame rate.
fn default_bindings_table() -> &'static HashMap<String, Vec<KeyBinding>> {
    static TABLE: std::sync::OnceLock<HashMap<String, Vec<KeyBinding>>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(default_bindings)
}

fn ssms_overrides_table() -> &'static HashMap<String, KeyBinding> {
    static TABLE: std::sync::OnceLock<HashMap<String, KeyBinding>> = std::sync::OnceLock::new();
    TABLE.get_or_init(ssms_overrides)
}

/// Display hint resolved from the shortcut snapshot registered at startup.
/// This runs on render paths and deliberately keeps advertising the bindings
/// the current process dispatches after Preferences saves a restart-required
/// change.
pub fn shortcut_hint(command_id: &str) -> String {
    if let Ok(cache) = HINT_CACHE.read()
        && let Some(hint) = cache.get(command_id)
    {
        return hint.clone();
    }
    let hint = {
        let guard = ACTIVE_SETTINGS.read().expect("hint cache lock poisoned");
        guard
            .as_ref()
            .map(|settings| hint_for_settings(command_id, settings))
    };
    let hint = match hint {
        Some(hint) => hint,
        None => {
            let settings = load_keybinding_settings(&keybindings_path());
            let hint = hint_for_settings(command_id, &settings);
            set_active_settings(Some(settings));
            hint
        }
    };
    if let Ok(mut cache) = HINT_CACHE.write() {
        cache.insert(command_id.to_owned(), hint.clone());
    }
    hint
}

/// Replace the process-wide active settings and drop every memoized hint,
/// so the next render re-resolves against the new snapshot.
fn set_active_settings(settings: Option<KeybindingSettings>) {
    if let Ok(mut guard) = ACTIVE_SETTINGS.write() {
        *guard = settings;
    }
    if let Ok(mut cache) = HINT_CACHE.write() {
        cache.clear();
    }
}

/// Resolved display hints by command id for the active settings. Rendering
/// resolves the same handful of hints every frame; resolving walks the
/// override/default/SSMS tables and formats keystrokes each time.
static HINT_CACHE: std::sync::LazyLock<std::sync::RwLock<HashMap<String, String>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(HashMap::new()));

/// In-memory copy of the settings registered for this process. Saves do not
/// alter it; startup replaces it after loading the next persisted snapshot.
static ACTIVE_SETTINGS: std::sync::RwLock<Option<KeybindingSettings>> =
    std::sync::RwLock::new(None);

// ââ Descriptor parsing + persistence âââââââââââââââââââââââââââââââââââââââ

/// Parse a single descriptor token (e.g. "cmd+shift+p", "f5", "fn+left") into
/// a `KeyStroke`. Returns `None` on an unknown key name.
fn parse_descriptor_stroke(token: &str) -> Option<KeyStroke> {
    let mut mods = Modifiers::none();
    let mut key_part = token;
    while let Some((head, rest)) = key_part.split_once('+') {
        match head {
            "cmd" => mods.command = true,
            "shift" => mods.shift = true,
            "option" | "alt" => mods.option = true,
            "ctrl" => mods.control = true,
            "fn" => {
                // "fn+left" is a single token where fn maps to a navigation key.
                // The remaining part is the key name; treat fn-modifier as part
                // of the key scalar.
                let nav_key = match rest {
                    "left" => HOME_KEY,
                    "right" => END_KEY,
                    "up" => PAGE_UP_KEY,
                    "down" => PAGE_DOWN_KEY,
                    _ => return None,
                };
                return Some(KeyStroke::new(nav_key, mods));
            }
            _ => break,
        }
        key_part = rest;
    }
    let k = match key_part {
        "space" => ' ',
        "enter" => '\r',
        "tab" => '\t',
        "backspace" => BACKSPACE_KEY,
        "delete" => DELETE_KEY,
        "escape" => ESC,
        "home" => HOME_KEY,
        "end" => END_KEY,
        "pageup" => PAGE_UP_KEY,
        "pagedown" => PAGE_DOWN_KEY,
        "left" => LEFT_KEY,
        "right" => RIGHT_KEY,
        "/" => '/',
        "." => '.',
        "," => ',',
        s if s.starts_with('f')
            && s[1..]
                .parse::<u32>()
                .is_ok_and(|number| (1..=20).contains(&number)) =>
        {
            let n = s[1..].parse::<u32>().unwrap();
            function_key_scalar(n)
        }
        s if s.chars().count() == 1 => s.chars().next()?,
        _ => return None,
    };
    Some(KeyStroke::new(k, mods))
}

/// Parse a full binding descriptor (one or two strokes joined by a space) into
/// a `KeyBinding`. Returns `None` on a parse failure.
fn parse_descriptor(descriptor: &str) -> Option<KeyBinding> {
    let tokens: Vec<&str> = descriptor.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    let strokes: Vec<KeyStroke> = tokens
        .iter()
        .map(|token| parse_descriptor_stroke(token))
        .collect::<Option<_>>()?;
    if strokes.is_empty() {
        None
    } else {
        Some(KeyBinding {
            strokes,
            when: None,
        })
    }
}

/// All app-level shortcut preferences persisted together so startup always
/// resolves a profile and its per-command overrides from one snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingSettings {
    pub profile: ShortcutProfile,
    pub overrides: KeyBindingOverrides,
}

pub fn keybindings_path() -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    return dirs::config_dir()
        .map(|directory| directory.join("SQLLY").join("keybindings.json"))
        .unwrap_or_else(|| PathBuf::from("keybindings.json"));
    #[cfg(target_arch = "wasm32")]
    return PathBuf::from("keybindings.json");
}

/// JSON shape persisted to disk. `profile` is optional so version-1 files
/// written before profiles became live continue to load as platform-native.
#[derive(serde::Deserialize, serde::Serialize)]
struct KeyBindingConfigFile {
    version: u32,
    #[serde(default)]
    profile: ShortcutProfile,
    bindings: Vec<KeyBindingConfigEntry>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct KeyBindingConfigEntry {
    command: String,
    key: String,
    /// Optional context expression restricting when the binding fires,
    /// validated against gpui's key-context predicate grammar on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    when: Option<String>,
}

/// Load the active profile and per-command overrides. Missing, unreadable, or
/// malformed files safely fall back to platform-native defaults.
pub fn load_keybinding_settings(path: &Path) -> KeybindingSettings {
    let registry = crate::command_palette::CommandRegistry::default_registry();
    let known: HashSet<String> = registry.all().iter().map(|c| c.id.clone()).collect();
    load_keybinding_settings_with(&known, path)
}

/// Same as [`load_keybinding_settings`], with a caller-supplied known-id set
/// so tests can keep a `quickAction.slotN` override without writing the
/// global snippets preferences file.
pub fn load_keybinding_settings_with(known: &HashSet<String>, path: &Path) -> KeybindingSettings {
    // Browser build: overrides live in localStorage (path is nominal).
    #[cfg(target_arch = "wasm32")]
    let contents = {
        let _ = path;
        match crate::host_storage::load_text(
            crate::host_storage::StorageArea::Config,
            "keybindings.json",
        ) {
            Some(text) => text,
            None => return KeybindingSettings::default(),
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    let Ok(contents) = std::fs::read_to_string(path) else {
        return KeybindingSettings::default();
    };
    let Ok(config) = serde_json::from_str::<KeyBindingConfigFile>(&contents) else {
        return KeybindingSettings::default();
    };
    let mut overrides = KeyBindingOverrides::new();
    for entry in config.bindings {
        if !known.contains(&entry.command) {
            continue;
        }
        let Some(mut binding) = parse_descriptor(&entry.key) else {
            continue;
        };
        binding.when = entry.when.as_deref().and_then(validate_when_clause);
        let slot = overrides.entry(entry.command).or_default();
        if slot.len() < MAX_BINDINGS_PER_COMMAND && !slot.contains(&binding) {
            slot.push(binding);
        }
    }
    KeybindingSettings {
        profile: config.profile,
        overrides,
    }
}

pub fn save_keybinding_settings(settings: &KeybindingSettings, path: &Path) -> std::io::Result<()> {
    let registry = crate::command_palette::CommandRegistry::default_registry();
    let commands = registry.all();
    let mut entries: Vec<KeyBindingConfigEntry> = Vec::new();
    for cmd in commands {
        if let Some(bindings) = settings.overrides.get(&cmd.id) {
            for binding in bindings.iter().take(MAX_BINDINGS_PER_COMMAND) {
                entries.push(KeyBindingConfigEntry {
                    command: cmd.id.clone(),
                    key: binding.descriptor_string(),
                    when: binding.when.clone(),
                });
            }
        }
    }
    let config = KeyBindingConfigFile {
        version: 2,
        profile: settings.profile,
        bindings: entries,
    };
    // Browser build: overrides live in localStorage (path is nominal).
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        let json = serde_json::to_string_pretty(&config)?;
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Config,
            "keybindings.json",
            &(json + "\n"),
        )
        .map_err(std::io::Error::other)
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&config)?;
        std::fs::write(path, json + "\n")
    }
}

// ââ KeybindingsEditorModel âââââââââââââââââââââââââââââââââââââââââââââââââ

/// `KeyBindingOverrides` â per-command custom bindings.
pub type KeyBindingOverrides = HashMap<String, Vec<KeyBinding>>;

/// One row in the keybindings editor, representing a single command.
#[derive(Debug, Clone)]
pub struct KeybindingsEditorRow {
    pub command: ShellCommand,
    pub default_bindings: Vec<KeyBinding>,
    pub active_bindings: Vec<KeyBinding>,
    pub override_bindings: Vec<KeyBinding>,
}

impl KeybindingsEditorRow {
    pub fn has_overrides(&self) -> bool {
        !self.override_bindings.is_empty()
    }

    pub fn default_display(&self) -> String {
        self.default_bindings
            .iter()
            .map(|b| b.display_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn active_display(&self) -> String {
        self.active_bindings
            .iter()
            .map(|b| b.display_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[allow(dead_code)]
    pub fn override_display(&self) -> String {
        self.override_bindings
            .iter()
            .map(|b| b.display_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Pure keybindings editor model. Owns the profile, the command list, and the
/// custom overrides; derives rows, conflicts, and active bindings from them.
#[derive(Debug, Clone)]
pub struct KeybindingsEditorModel {
    pub profile: ShortcutProfile,
    pub commands: Vec<ShellCommand>,
    pub overrides: KeyBindingOverrides,
    defaults: HashMap<String, Vec<KeyBinding>>,
    ssms: HashMap<String, KeyBinding>,
}

impl KeybindingsEditorModel {
    pub fn new(
        profile: ShortcutProfile,
        commands: Vec<ShellCommand>,
        overrides: KeyBindingOverrides,
    ) -> Self {
        let defaults = default_bindings();
        let ssms = ssms_overrides();
        let mut model = Self {
            profile,
            commands,
            overrides,
            defaults,
            ssms,
        };
        model.sanitize_overrides();
        model
    }

    #[allow(dead_code)]
    /// Creates a model from the default command registry.
    pub fn from_default_registry(profile: ShortcutProfile) -> Self {
        let commands = crate::command_palette::CommandRegistry::default_registry()
            .all()
            .to_vec();
        Self::new(profile, commands, HashMap::new())
    }

    /// Drops overrides for unknown command ids, dedupes bindings within each
    /// command, and caps at `MAX_BINDINGS_PER_COMMAND`.
    fn sanitize_overrides(&mut self) {
        let known: std::collections::HashSet<&str> =
            self.commands.iter().map(|c| c.id.as_str()).collect();
        let defaults = &self.defaults;
        let ssms = &self.ssms;
        self.overrides.retain(|id, _| known.contains(id.as_str()));
        for bindings in self.overrides.values_mut() {
            dedupe(bindings);
            bindings.truncate(MAX_BINDINGS_PER_COMMAND);
        }
        // Suppress unused warnings for defaults/ssms in this closure context.
        let _ = (defaults, ssms);
    }

    /// Filtered, sorted rows for the editor list.
    pub fn rows(&self, query: &str) -> Vec<KeybindingsEditorRow> {
        let q = query.trim();
        self.commands
            .iter()
            .filter(|c| q.is_empty() || matches_command(c, q))
            .map(|c| self.row_for(c.clone()))
            .collect()
    }

    pub fn set_profile(&mut self, profile: ShortcutProfile) {
        self.profile = profile;
    }

    pub fn active_conflicts(&self) -> Vec<KeybindingConflict> {
        detect_conflicts(&resolved_app_bindings(&KeybindingSettings {
            profile: self.profile,
            overrides: self.overrides.clone(),
        }))
    }

    /// One row for a specific command id.
    pub fn row_for(&self, command: ShellCommand) -> KeybindingsEditorRow {
        let default_bindings = self.defaults.get(&command.id).cloned().unwrap_or_default();
        let active_bindings = resolve_bindings(
            &command.id,
            self.profile,
            &self.overrides,
            &self.defaults,
            &self.ssms,
        );
        let override_bindings = self.overrides.get(&command.id).cloned().unwrap_or_default();
        KeybindingsEditorRow {
            command,
            default_bindings,
            active_bindings,
            override_bindings,
        }
    }

    /// Sets a binding in a specific slot (0-based) for a command. Dedupes and
    /// caps at `MAX_BINDINGS_PER_COMMAND`.
    #[allow(dead_code)]
    pub fn set_binding(&mut self, binding: KeyBinding, command_id: &str, slot: usize) {
        let entry = self.overrides.entry(command_id.to_string()).or_default();
        if slot < entry.len() {
            entry[slot] = binding;
        } else {
            entry.push(binding);
        }
        dedupe(entry);
        entry.truncate(MAX_BINDINGS_PER_COMMAND);
        if entry.is_empty() {
            self.overrides.remove(command_id);
        }
    }

    #[allow(dead_code)]
    /// Clears the binding at a specific slot for a command.
    pub fn clear_binding(&mut self, command_id: &str, slot: usize) {
        if let Some(entry) = self.overrides.get_mut(command_id) {
            if slot < entry.len() {
                entry.remove(slot);
            }
            if entry.is_empty() {
                self.overrides.remove(command_id);
            }
        }
    }

    /// Resets a single command to its default (removes all overrides).
    pub fn reset(&mut self, command_id: &str) {
        self.overrides.remove(command_id);
    }

    /// Resets all commands to their defaults.
    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    #[allow(dead_code)]
    /// Finds other commands whose active bindings contain `binding` (excluding
    /// the command at `command_id`).
    pub fn conflicts(&self, binding: &KeyBinding, command_id: &str) -> Vec<ShellCommand> {
        self.commands
            .iter()
            .filter(|c| c.id != command_id)
            .filter(|c| {
                let active = resolve_bindings(
                    &c.id,
                    self.profile,
                    &self.overrides,
                    &self.defaults,
                    &self.ssms,
                );
                active.iter().any(|b| b == binding)
            })
            .cloned()
            .collect()
    }

    #[allow(dead_code)]
    /// Whether any command has overrides.
    pub fn has_any_overrides(&self) -> bool {
        !self.overrides.is_empty()
    }
}

/// Case-insensitive match against a command's title, id, category label, and
/// keywords.
fn matches_command(command: &ShellCommand, query: &str) -> bool {
    let q = query.to_lowercase();
    if command.title.to_lowercase().contains(&q) {
        return true;
    }
    if command.id.to_lowercase().contains(&q) {
        return true;
    }
    if command.category.label().to_lowercase().contains(&q) {
        return true;
    }
    command
        .keywords
        .iter()
        .any(|kw| kw.to_lowercase().contains(&q))
}

/// Remove duplicate bindings (preserving order).
fn dedupe(bindings: &mut Vec<KeyBinding>) {
    let mut seen = std::collections::HashSet::new();
    bindings.retain(|b| seen.insert(b.clone()));
}

// ââ Tests ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_palette::CommandCategory;

    #[cfg(target_os = "macos")]
    const PRIMARY_STROKE: &str = "cmd";
    #[cfg(not(target_os = "macos"))]
    const PRIMARY_STROKE: &str = "ctrl";

    fn cmd(id: &str, title: &str) -> ShellCommand {
        ShellCommand {
            id: id.to_string(),
            title: title.to_string(),
            category: CommandCategory::Query,
            keywords: vec![],
        }
    }

    // KeyStroke display

    #[test]
    fn keystroke_display_basic() {
        let ks = KeyStroke::new('p', Modifiers::cmd_shift());
        assert_eq!(ks.display_string(), format!("{PRIMARY_LABEL}+Shift+P"));
    }

    #[test]
    fn keystroke_display_command_only() {
        let ks = KeyStroke::new('n', Modifiers::command());
        assert_eq!(ks.display_string(), format!("{PRIMARY_LABEL}+N"));
    }

    #[test]
    fn keystroke_display_no_modifiers() {
        let ks = KeyStroke::function_key(5);
        assert_eq!(ks.display_string(), "F5");
    }

    #[test]
    fn keystroke_display_enter() {
        let ks = KeyStroke::new('\r', Modifiers::command());
        assert_eq!(ks.display_string(), format!("{PRIMARY_LABEL}+Enter"));
    }

    #[test]
    fn keystroke_display_tab() {
        let ks = KeyStroke::new('\t', Modifiers::none());
        assert_eq!(ks.display_string(), "Tab");
    }

    #[test]
    fn keystroke_display_space() {
        let ks = KeyStroke::new(' ', Modifiers::none());
        assert_eq!(ks.display_string(), "Space");
    }

    #[test]
    fn keystroke_display_escape() {
        let ks = KeyStroke::new(ESC, Modifiers::command());
        assert_eq!(ks.display_string(), format!("{PRIMARY_LABEL}+Esc"));
    }

    #[test]
    fn keystroke_display_navigation_keys() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(KeyStroke::home_key().display_string(), "fn+â");
            assert_eq!(KeyStroke::end_key().display_string(), "fn+â");
            assert_eq!(KeyStroke::page_up_key().display_string(), "fn+â");
            assert_eq!(KeyStroke::page_down_key().display_string(), "fn+â");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(KeyStroke::home_key().display_string(), "Home");
            assert_eq!(KeyStroke::end_key().display_string(), "End");
            assert_eq!(KeyStroke::page_up_key().display_string(), "Page Up");
            assert_eq!(KeyStroke::page_down_key().display_string(), "Page Down");
        }
    }

    #[test]
    fn keystroke_display_all_function_keys() {
        for n in 1..=20u32 {
            let ks = KeyStroke::function_key(n);
            assert_eq!(ks.display_string(), format!("F{}", n));
        }
    }

    #[test]
    fn keystroke_display_lowercase_normalized() {
        let ks = KeyStroke::new('P', Modifiers::command());
        assert_eq!(ks.display_string(), format!("{PRIMARY_LABEL}+P"));
    }

    // KeyStroke descriptor

    #[test]
    fn keystroke_descriptor_basic() {
        let ks = KeyStroke::new('p', Modifiers::cmd_shift());
        // The descriptor is the canonical persisted form: `command` always
        // serialises as `cmd`, independent of platform (Ctrl is display-only).
        assert_eq!(ks.descriptor_string(), "cmd+shift+p");
    }

    #[test]
    fn descriptor_round_trips_command_modifier() {
        // Regression: on non-macOS the descriptor once serialised `command` as
        // `ctrl`, which parsed back to `control` and broke persistence
        // round-trips (and cross-OS moves). The descriptor is canonical, so a
        // command binding must survive descriptor_string â parse unchanged on
        // every platform.
        let binding = KeyBinding::new('r', Modifiers::command());
        let restored = parse_descriptor(&binding.descriptor_string());
        assert_eq!(restored.as_ref(), Some(&binding));
    }

    #[test]
    fn keystroke_descriptor_function_key() {
        let ks = KeyStroke::function_key(5);
        assert_eq!(ks.descriptor_string(), "f5");
    }

    #[test]
    fn keystroke_descriptor_navigation() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(KeyStroke::home_key().descriptor_string(), "fn+left");
            assert_eq!(KeyStroke::end_key().descriptor_string(), "fn+right");
            assert_eq!(KeyStroke::page_up_key().descriptor_string(), "fn+up");
            assert_eq!(KeyStroke::page_down_key().descriptor_string(), "fn+down");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(KeyStroke::home_key().descriptor_string(), "home");
            assert_eq!(KeyStroke::end_key().descriptor_string(), "end");
            assert_eq!(KeyStroke::page_up_key().descriptor_string(), "pageup");
            assert_eq!(KeyStroke::page_down_key().descriptor_string(), "pagedown");
        }
    }

    #[test]
    fn keystroke_descriptor_special_chars() {
        assert_eq!(
            KeyStroke::new(' ', Modifiers::none()).descriptor_string(),
            "space"
        );
        assert_eq!(
            KeyStroke::new('\r', Modifiers::none()).descriptor_string(),
            "enter"
        );
        assert_eq!(
            KeyStroke::new('\t', Modifiers::none()).descriptor_string(),
            "tab"
        );
        assert_eq!(
            KeyStroke::new('/', Modifiers::none()).descriptor_string(),
            "/"
        );
        assert_eq!(
            KeyStroke::new('.', Modifiers::none()).descriptor_string(),
            "."
        );
        assert_eq!(
            KeyStroke::new(',', Modifiers::none()).descriptor_string(),
            ","
        );
        assert_eq!(
            KeyStroke::new(ESC, Modifiers::none()).descriptor_string(),
            "escape"
        );
    }

    // KeyBinding display

    #[test]
    fn keybinding_single_display() {
        let kb = KeyBinding::new('p', Modifiers::cmd_shift());
        assert_eq!(kb.display_string(), format!("{PRIMARY_LABEL}+Shift+P"));
        assert!(kb.is_single_stroke());
    }

    #[test]
    fn keybinding_multi_stroke_display() {
        let kb = KeyBinding {
            strokes: vec![
                KeyStroke::new('k', Modifiers::command()),
                KeyStroke::new('c', Modifiers::command()),
            ],
            when: None,
        };
        assert_eq!(
            kb.display_string(),
            format!("{PRIMARY_LABEL}+K {PRIMARY_LABEL}+C")
        );
        assert!(!kb.is_single_stroke());
    }

    // ShortcutProfile

    #[test]
    fn profile_labels() {
        #[cfg(target_os = "macos")]
        assert_eq!(ShortcutProfile::MacOSNative.label(), "macOS Native");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(ShortcutProfile::MacOSNative.label(), "Platform Native");
        assert_eq!(ShortcutProfile::Ssms.label(), "SSMS");
        assert_eq!(ShortcutProfile::Custom.label(), "Custom");
    }

    // default_bindings / ssms_overrides

    #[test]
    fn default_bindings_populated() {
        let d = default_bindings();
        assert_eq!(
            d.get("file.newQuery").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+N")
        );
        assert_eq!(
            d.get("query.execute").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+Enter")
        );
        assert_eq!(
            d.get("query.validate").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+Shift+V")
        );
        assert_eq!(
            d.get("tools.commandPalette").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+Shift+P")
        );
        assert_eq!(
            d.get("tools.quickOpen").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+P")
        );
        let conflicts = detect_conflicts(&d);
        assert!(
            !conflicts.iter().any(|conflict| {
                conflict
                    .command_ids
                    .iter()
                    .any(|id| id == "tools.quickOpen")
            }),
            "tools.quickOpen default chord must not collide: {conflicts:?}"
        );
        assert_eq!(
            d.get("view.toggleResultsPane").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+J")
        );
        assert_eq!(
            d.get("view.toggleResultsMaximized").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+Shift+J")
        );
        assert_eq!(
            d.get("view.selectOutputResults").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+1")
        );
        assert_eq!(
            d.get("view.selectOutputMessages").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+2")
        );
        assert_eq!(
            d.get("view.selectOutputPlan").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+3")
        );
        assert_eq!(
            d.get("view.selectOutputHistory").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+4")
        );
        assert_eq!(
            d.get("view.selectOutputDdl").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+5")
        );
        assert_eq!(
            d.get("view.selectOutputDiff").unwrap()[0].display_string(),
            format!("{PRIMARY_LABEL}+6")
        );
    }

    #[test]
    fn default_bindings_results_pane_commands_are_scoped() {
        let d = default_bindings();
        for id in [
            "results.findInGrid",
            "results.gotoColumn",
            "results.stageInsertRow",
            "results.stageDeleteRow",
            "results.refresh",
            "results.stage_undo",
            "results.stage_redo",
        ] {
            let bindings = d.get(id).unwrap_or_else(|| panic!("missing {id}"));
            assert!(!bindings.is_empty(), "{id} must have a default binding");
            assert!(
                bindings
                    .iter()
                    .all(|b| b.when.as_deref() == Some("ResultsPane")),
                "{id} must be scoped to ResultsPane, got {bindings:?}"
            );
        }
    }

    #[test]
    fn focus_toggle_binding_fires_from_editor_and_results_contexts() {
        let d = default_bindings();
        let bindings = d
            .get("results.focusToggle")
            .expect("missing results.focusToggle");
        assert_eq!(bindings.len(), 2, "one binding per key context");
        let contexts: Vec<Option<&str>> = bindings.iter().map(|b| b.when.as_deref()).collect();
        assert!(
            contexts.contains(&Some("SqlEditor")),
            "F6 must fire from the editor: {bindings:?}"
        );
        assert!(
            contexts.contains(&Some("ResultsPane")),
            "F6 must fire from the results grid: {bindings:?}"
        );
        assert!(
            bindings
                .iter()
                .all(|b| b.strokes.as_slice() == [KeyStroke::function_key(6)]),
            "both bindings are F6: {bindings:?}"
        );
    }

    #[test]
    fn send_selection_binding_is_unique() {
        let defaults = default_bindings();
        let expected = KeyBinding::new('a', Modifiers::cmd_shift()).display_string();
        let hits: Vec<&str> = defaults
            .iter()
            .filter(|(_, binds)| binds.iter().any(|bind| bind.display_string() == expected))
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(hits, vec!["query.sendSelectionToAi"]);
        for bind in ssms_overrides().values() {
            assert_ne!(bind.display_string(), expected);
        }
    }

    #[test]
    fn ssms_overrides_populated() {
        let s = ssms_overrides();
        assert_eq!(s.get("query.execute").unwrap().display_string(), "F5");
        assert!(!s.contains_key("query.cancel"));
    }

    // Resolver

    #[test]
    fn resolver_falls_back_to_defaults() {
        let d = default_bindings();
        let s = ssms_overrides();
        let bindings = resolve_bindings(
            "query.execute",
            ShortcutProfile::MacOSNative,
            &HashMap::new(),
            &d,
            &s,
        );
        assert_eq!(
            bindings[0].display_string(),
            format!("{PRIMARY_LABEL}+Enter")
        );
    }

    #[test]
    fn resolver_uses_ssms_override() {
        let d = default_bindings();
        let s = ssms_overrides();
        let bindings = resolve_bindings(
            "query.execute",
            ShortcutProfile::Ssms,
            &HashMap::new(),
            &d,
            &s,
        );
        assert_eq!(bindings[0].display_string(), "F5");
    }

    #[test]
    fn resolver_custom_overrides_win() {
        let d = default_bindings();
        let s = ssms_overrides();
        let mut overrides = HashMap::new();
        overrides.insert(
            "query.execute".into(),
            vec![KeyBinding::new('r', Modifiers::command())],
        );
        let bindings = resolve_bindings("query.execute", ShortcutProfile::Ssms, &overrides, &d, &s);
        assert_eq!(bindings[0].display_string(), format!("{PRIMARY_LABEL}+R"));
    }

    // Shortcut hints for empty states / welcome screen

    #[test]
    fn hint_for_uses_default_when_unbound() {
        assert_eq!(
            hint_for("query.execute", &HashMap::new()),
            format!("{PRIMARY_LABEL}+Enter, F5")
        );
        assert_eq!(
            hint_for("tools.commandPalette", &HashMap::new()),
            format!("{PRIMARY_LABEL}+Shift+P")
        );
    }

    #[test]
    fn hint_for_reflects_rebind() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "query.execute".into(),
            vec![KeyBinding::new('e', Modifiers::cmd_shift())],
        );
        assert_eq!(
            hint_for("query.execute", &overrides),
            format!("{PRIMARY_LABEL}+Shift+E")
        );
    }

    #[test]
    fn hint_for_unknown_command_is_empty() {
        assert_eq!(hint_for("no.such.command", &HashMap::new()), "");
    }

    #[test]
    fn saved_changes_do_not_relabel_the_active_map_before_restart() {
        set_active_settings(Some(KeybindingSettings::default()));
        let dir = std::env::temp_dir().join(format!("sqlly-kb-hint-{}", std::process::id()));
        let path = dir.join("keybindings.json");
        let saved = KeybindingSettings {
            profile: ShortcutProfile::Ssms,
            overrides: HashMap::new(),
        };
        save_keybinding_settings(&saved, &path).unwrap();
        assert_eq!(
            ACTIVE_SETTINGS.read().unwrap().as_ref().unwrap().profile,
            ShortcutProfile::MacOSNative
        );
        set_active_settings(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_round_trip_profile_and_overrides() {
        let dir = std::env::temp_dir().join(format!(
            "sqlly-kb-settings-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("keybindings.json");
        let mut overrides = HashMap::new();
        overrides.insert(
            "query.execute".to_string(),
            vec![KeyBinding::new('r', Modifiers::command())],
        );
        let settings = KeybindingSettings {
            profile: ShortcutProfile::Ssms,
            overrides,
        };
        save_keybinding_settings(&settings, &path).unwrap();
        assert_eq!(load_keybinding_settings(&path), settings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_clauses_round_trip_and_invalid_ones_are_dropped() {
        let dir = std::env::temp_dir().join(format!(
            "sqlly-kb-when-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("keybindings.json");
        let mut binding = KeyBinding::new('r', Modifiers::command());
        binding.when = Some("SqlEditor".to_string());
        let mut overrides = HashMap::new();
        overrides.insert("query.execute".to_string(), vec![binding]);
        let settings = KeybindingSettings {
            profile: ShortcutProfile::MacOSNative,
            overrides,
        };
        save_keybinding_settings(&settings, &path).unwrap();
        let loaded = load_keybinding_settings(&path);
        assert_eq!(loaded, settings, "valid when clause survives save/load");
        let specs = runtime_binding_specs(&loaded);
        let exec = specs
            .iter()
            .find(|(id, _, _)| id == "query.execute")
            .expect("binding present");
        assert_eq!(exec.2.as_deref(), Some("SqlEditor"));

        // An invalid predicate is dropped (binding stays, global scope).
        std::fs::write(
            &path,
            r#"{"version":2,"bindings":[{"command":"query.execute","key":"f5","when":"(("}]}"#,
        )
        .unwrap();
        let loaded = load_keybinding_settings(&path);
        let binding = &loaded.overrides["query.execute"][0];
        assert_eq!(binding.when, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_settings_without_profile_load_platform_native() {
        let dir = std::env::temp_dir().join(format!(
            "sqlly-kb-legacy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("keybindings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"bindings":[{"command":"query.execute","key":"f5"}]}"#,
        )
        .unwrap();
        let settings = load_keybinding_settings(&path);
        assert_eq!(settings.profile, ShortcutProfile::MacOSNative);
        assert_eq!(
            settings.overrides["query.execute"][0],
            KeyBinding::single(KeyStroke::function_key(5))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolved_map_reports_cross_command_conflicts() {
        let duplicate = KeyBinding::new('r', Modifiers::command());
        let mut settings = KeybindingSettings::default();
        settings
            .overrides
            .insert("query.execute".into(), vec![duplicate.clone()]);
        settings
            .overrides
            .insert("query.validate".into(), vec![duplicate.clone()]);
        let conflicts = detect_conflicts(&resolved_app_bindings(&settings));
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].binding, duplicate);
        assert_eq!(
            conflicts[0].command_ids,
            vec!["query.execute", "query.validate"]
        );
    }

    #[test]
    fn runtime_specs_apply_profile_then_per_command_override() {
        let ssms = KeybindingSettings {
            profile: ShortcutProfile::Ssms,
            overrides: HashMap::new(),
        };
        let ssms_execute: Vec<_> = runtime_binding_specs(&ssms)
            .into_iter()
            .filter(|(command, _, _)| command == "query.execute")
            .collect();
        assert_eq!(
            ssms_execute,
            vec![("query.execute".into(), "f5".into(), None)]
        );

        let mut custom = ssms;
        custom.overrides.insert(
            "query.execute".into(),
            vec![KeyBinding::new('r', Modifiers::command())],
        );
        let custom_execute: Vec<_> = runtime_binding_specs(&custom)
            .into_iter()
            .filter(|(command, _, _)| command == "query.execute")
            .collect();
        assert_eq!(
            custom_execute,
            vec![("query.execute".into(), format!("{PRIMARY_STROKE}-r"), None)]
        );
    }

    #[gpui::test]
    fn startup_registration_installs_resolved_action(cx: &mut gpui::TestAppContext) {
        let mut settings = KeybindingSettings::default();
        settings.overrides.insert(
            "query.execute".into(),
            vec![KeyBinding::new('r', Modifiers::command())],
        );
        cx.update(|cx| {
            register_app_keybindings_from(&settings, cx);
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let expected = parse_keystroke(&format!("{PRIMARY_STROKE}-r")).unwrap();
            assert!(keymap.bindings().any(|binding| {
                binding
                    .action()
                    .as_any()
                    .is::<crate::app::ExecuteActiveQuery>()
                    && binding.match_keystrokes(std::slice::from_ref(&expected)) == Some(false)
            }));
        });
    }

    #[test]
    fn resolver_caps_at_max() {
        let d = default_bindings();
        let s = ssms_overrides();
        let mut overrides = HashMap::new();
        overrides.insert(
            "file.newQuery".into(),
            vec![
                KeyBinding::new('a', Modifiers::command()),
                KeyBinding::new('b', Modifiers::command()),
                KeyBinding::new('c', Modifiers::command()),
                KeyBinding::new('d', Modifiers::command()),
                KeyBinding::new('e', Modifiers::command()),
            ],
        );
        let bindings = resolve_bindings(
            "file.newQuery",
            ShortcutProfile::MacOSNative,
            &overrides,
            &d,
            &s,
        );
        assert_eq!(bindings.len(), MAX_BINDINGS_PER_COMMAND);
    }

    // KeybindingsEditorModel

    #[test]
    fn model_rows_empty_query_returns_all() {
        let model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        let rows = model.rows("");
        assert!(!rows.is_empty());
    }

    #[test]
    fn model_rows_filtered_by_query() {
        let model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        let rows = model.rows("execute");
        assert!(
            rows.iter()
                .all(|r| r.command.title.to_lowercase().contains("execute")
                    || r.command.id.to_lowercase().contains("execute")
                    || r.command.keywords.iter().any(|k| k.contains("execute")))
        );
    }

    #[test]
    fn model_row_shows_default_when_no_override() {
        let model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        let row = model.row_for(cmd("query.execute", "Execute Query"));
        assert!(!row.default_display().is_empty());
        assert_eq!(row.default_display(), row.active_display());
        assert!(!row.has_overrides());
    }

    #[test]
    fn model_set_binding_creates_override() {
        let mut model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        let binding = KeyBinding::new('r', Modifiers::command());
        model.set_binding(binding.clone(), "query.execute", 0);
        let row = model.row_for(cmd("query.execute", "Execute Query"));
        assert!(row.has_overrides());
        assert_eq!(row.active_display(), format!("{PRIMARY_LABEL}+R"));
    }

    #[test]
    fn model_clear_binding_removes_override() {
        let mut model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        model.set_binding(
            KeyBinding::new('r', Modifiers::command()),
            "query.execute",
            0,
        );
        assert!(model.has_any_overrides());
        model.clear_binding("query.execute", 0);
        assert!(!model.has_any_overrides());
    }

    #[test]
    fn model_reset_command() {
        let mut model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        model.set_binding(
            KeyBinding::new('r', Modifiers::command()),
            "query.execute",
            0,
        );
        model.reset("query.execute");
        assert!(!model.has_any_overrides());
    }

    #[test]
    fn model_reset_all() {
        let mut model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        model.set_binding(
            KeyBinding::new('r', Modifiers::command()),
            "query.execute",
            0,
        );
        model.set_binding(
            KeyBinding::new('x', Modifiers::command()),
            "query.validate",
            0,
        );
        model.reset_all();
        assert!(!model.has_any_overrides());
    }

    #[test]
    fn model_sanitize_drops_unknown_ids() {
        let commands = vec![cmd("query.execute", "Execute Query")];
        let mut overrides = HashMap::new();
        overrides.insert(
            "nonexistent.command".into(),
            vec![KeyBinding::new('x', Modifiers::command())],
        );
        let model = KeybindingsEditorModel::new(ShortcutProfile::MacOSNative, commands, overrides);
        assert!(model.overrides.is_empty());
    }

    #[test]
    fn model_sanitize_dedupes_bindings() {
        let commands = vec![cmd("query.execute", "Execute Query")];
        let mut overrides = HashMap::new();
        let dup = KeyBinding::new('r', Modifiers::command());
        overrides.insert("query.execute".into(), vec![dup.clone(), dup.clone()]);
        let model = KeybindingsEditorModel::new(ShortcutProfile::MacOSNative, commands, overrides);
        assert_eq!(model.overrides.get("query.execute").unwrap().len(), 1);
    }

    #[test]
    fn model_conflicts_finds_clash() {
        // query.execute default is Cmd+Enter; set Cmd+Enter on query.cancel â conflict.
        let model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        let cmd_enter = KeyBinding::new('\r', Modifiers::command());
        let conflicts = model.conflicts(&cmd_enter, "query.cancel");
        let ids: Vec<&str> = conflicts.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"query.execute"));
    }

    #[test]
    fn model_conflicts_excludes_self() {
        let model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::MacOSNative);
        let cmd_enter = KeyBinding::new('\r', Modifiers::command());
        let conflicts = model.conflicts(&cmd_enter, "query.execute");
        assert!(!conflicts.iter().any(|c| c.id == "query.execute"));
    }

    #[test]
    fn model_ssms_profile_shows_override_in_active() {
        let model = KeybindingsEditorModel::from_default_registry(ShortcutProfile::Ssms);
        let row = model.row_for(cmd("query.execute", "Execute Query"));
        assert_eq!(row.active_display(), "F5");
        assert_eq!(row.default_display(), format!("{PRIMARY_LABEL}+Enter, F5"));
        assert!(!row.has_overrides());
    }

    // from_gpui_keystroke conversion

    fn gpui_mods(platform: bool, shift: bool, alt: bool, control: bool) -> gpui::Modifiers {
        gpui::Modifiers {
            platform,
            shift,
            alt,
            control,
            function: false,
        }
    }

    fn gpui_ks(key: &str, mods: gpui::Modifiers) -> gpui::Keystroke {
        gpui::Keystroke {
            key: key.to_string(),
            modifiers: mods,
            key_char: None,
        }
    }

    #[test]
    fn gpui_conversion_cmd_c() {
        #[cfg(target_os = "macos")]
        let ks = gpui_ks("c", gpui_mods(true, false, false, false));
        #[cfg(not(target_os = "macos"))]
        let ks = gpui_ks("c", gpui_mods(false, false, false, true));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, 'c');
        assert!(app_ks.modifiers.command);
        assert!(!app_ks.modifiers.shift);
        assert!(!app_ks.modifiers.option);
        assert!(!app_ks.modifiers.control);
    }

    #[test]
    fn gpui_conversion_cmd_shift_v() {
        #[cfg(target_os = "macos")]
        let ks = gpui_ks("v", gpui_mods(true, true, false, false));
        #[cfg(not(target_os = "macos"))]
        let ks = gpui_ks("v", gpui_mods(false, true, false, true));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, 'v');
        assert!(app_ks.modifiers.command);
        assert!(app_ks.modifiers.shift);
    }

    #[test]
    fn gpui_conversion_enter() {
        #[cfg(target_os = "macos")]
        let ks = gpui_ks("enter", gpui_mods(true, false, false, false));
        #[cfg(not(target_os = "macos"))]
        let ks = gpui_ks("enter", gpui_mods(false, false, false, true));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, '\r');
        assert!(app_ks.modifiers.command);
    }

    #[test]
    fn gpui_conversion_escape() {
        let ks = gpui_ks("escape", gpui_mods(false, false, false, false));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, ESC);
    }

    #[test]
    fn gpui_conversion_f5() {
        let ks = gpui_ks("f5", gpui_mods(false, false, false, false));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, function_key_scalar(5));
    }

    #[test]
    fn gpui_conversion_home() {
        let ks = gpui_ks("home", gpui_mods(false, false, false, false));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, HOME_KEY);
    }

    #[test]
    fn gpui_conversion_rejects_bare_modifiers() {
        // "cmd" alone has no key to bind.
        let ks = gpui_ks("cmd", gpui_mods(true, false, false, false));
        assert!(from_gpui_keystroke(&ks).is_none());
    }

    #[test]
    fn gpui_conversion_rejects_multi_char_keys() {
        // "insert" is a valid gpui key but not mapped in the app keybinding system.
        let ks = gpui_ks("insert", gpui_mods(false, false, false, false));
        assert!(from_gpui_keystroke(&ks).is_none());
    }

    #[test]
    fn gpui_conversion_maps_arrow_keys() {
        let left = gpui_ks("left", gpui_mods(false, false, false, false));
        assert_eq!(from_gpui_keystroke(&left).unwrap().key, LEFT_KEY);
        let right = gpui_ks("right", gpui_mods(false, false, false, false));
        assert_eq!(from_gpui_keystroke(&right).unwrap().key, RIGHT_KEY);
    }

    #[test]
    fn cell_details_default_shortcut_is_free() {
        let d = default_bindings();
        let binding = &d.get("view.toggleCellDetails").unwrap()[0];
        assert_eq!(
            binding.display_string(),
            KeyBinding::new('i', Modifiers::cmd_option()).display_string()
        );
        for profile in [ShortcutProfile::MacOSNative, ShortcutProfile::Ssms] {
            let settings = KeybindingSettings {
                profile,
                ..KeybindingSettings::default()
            };
            let conflicts = detect_conflicts(&resolved_app_bindings(&settings));
            assert!(
                !conflicts.iter().any(|c| c
                    .command_ids
                    .iter()
                    .any(|id| id == "view.toggleCellDetails")),
                "{profile:?}: {conflicts:?}"
            );
        }
        let editor_chords: Vec<String> = crate::editor_keybindings::default_bindings()
            .into_values()
            .flatten()
            .collect();
        for taken in ["cmd-alt-i", "alt-cmd-i", "ctrl-alt-i", "alt-ctrl-i"] {
            assert!(
                !editor_chords.iter().any(|chord| chord == taken),
                "the editor binds {taken}"
            );
        }
    }

    #[test]
    fn default_bindings_run_picker_is_cmd_shift_enter() {
        let d = default_bindings();
        let binding = &d.get("query.runPicker").unwrap()[0];
        assert_eq!(binding.strokes[0].key, '\r');
        assert!(binding.strokes[0].modifiers.command);
        assert!(binding.strokes[0].modifiers.shift);
    }

    #[test]
    fn default_bindings_tab_history_uses_ctrl_alt_arrows() {
        let d = default_bindings();
        let prev = &d.get("window.previousTabInHistory").unwrap()[0];
        assert_eq!(prev.strokes[0].key, LEFT_KEY);
        assert!(prev.strokes[0].modifiers.control);
        assert!(prev.strokes[0].modifiers.option);
        assert!(!prev.strokes[0].modifiers.command);
        let next = &d.get("window.nextTabInHistory").unwrap()[0];
        assert_eq!(next.strokes[0].key, RIGHT_KEY);
        assert!(next.strokes[0].modifiers.control);
        assert!(next.strokes[0].modifiers.option);
        assert!(!next.strokes[0].modifiers.command);
    }

    #[test]
    fn default_bindings_quick_switch_uses_ctrl_shift_digits() {
        let d = default_bindings();
        let conflicts = detect_conflicts(&d);
        for n in 1u8..=9 {
            let id = format!("connection.quickSwitch{n}");
            let binding = &d.get(&id).expect("quick-switch default")[0];
            assert_eq!(binding.strokes[0].key, char::from(b'0' + n));
            assert!(binding.strokes[0].modifiers.control);
            assert!(binding.strokes[0].modifiers.shift);
            assert!(!binding.strokes[0].modifiers.command);
            assert!(
                !conflicts.iter().any(|conflict| conflict
                    .command_ids
                    .iter()
                    .any(|command| command == &id)
                    && conflict.command_ids.len() > 1),
                "{id} conflicts with another default: {conflicts:?}"
            );
        }
    }

    #[test]
    fn gpui_conversion_ctrl_alt() {
        let ks = gpui_ks("x", gpui_mods(false, false, true, true));
        let app_ks = from_gpui_keystroke(&ks).unwrap();
        assert_eq!(app_ks.key, 'x');
        assert!(app_ks.modifiers.option);
        #[cfg(target_os = "macos")]
        assert!(app_ks.modifiers.control);
        #[cfg(not(target_os = "macos"))]
        assert!(app_ks.modifiers.command);
        #[cfg(target_os = "macos")]
        assert!(!app_ks.modifiers.command);
        #[cfg(not(target_os = "macos"))]
        assert!(!app_ks.modifiers.control);
    }

    #[test]
    fn quick_action_slot_ids_register_bindings() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "quickAction.slot1".to_string(),
            vec![KeyBinding::new('1', Modifiers::cmd_shift())],
        );
        let settings = KeybindingSettings {
            profile: ShortcutProfile::MacOSNative,
            overrides,
        };
        let specs = runtime_binding_specs(&settings);
        assert!(
            specs.iter().any(|(id, _, _)| id == "quickAction.slot1"),
            "slot override missing from runtime specs: {specs:?}"
        );

        let dir = std::env::temp_dir().join(format!(
            "sqlly-kb-qa-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("keybindings.json");
        let _ = std::fs::create_dir_all(&dir);
        let key = KeyBinding::new('1', Modifiers::cmd_shift()).descriptor_string();
        std::fs::write(
            &path,
            format!(
                r#"{{"version":2,"bindings":[{{"command":"quickAction.slot1","key":"{key}"}}]}}"#
            ),
        )
        .unwrap();
        let mut known = HashSet::new();
        known.insert("quickAction.slot1".to_string());
        let loaded = load_keybinding_settings_with(&known, &path);
        assert!(
            loaded.overrides.contains_key("quickAction.slot1"),
            "configured slot override must survive load: {:?}",
            loaded.overrides
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
