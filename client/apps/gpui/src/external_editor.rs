//! External editor launcher — open a text buffer in the user's preferred
//! editor (VS Code, Zed, Sublime, Nova, nano, vi, or a custom binary).
//!
//! Port of `ExternalEditorLauncher.swift` + `SQLLYExports/ExternalEditorPreferences.swift`.
//! The pure resolution/quoting/pruning logic is unit-testable; the I/O layer
//! (`write_temp_file`, `open`, `open_stdin`, `launch`, `launch_in_terminal`)
//! is kept thin and delegates to `std::process::Command` / `osascript` (macOS)
//! / `x-terminal-emulator` (Linux) / `cmd.exe` (Windows).
//!
//! Editor binary paths are platform-specific: macOS prefers `/opt/homebrew/bin`
//! (Apple Silicon) before `/usr/local/bin`, Linux uses `/usr/bin`, and Windows
//! uses `Program Files`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Which built-in editor the user has selected. `Other` defers to a custom
/// binary path the user supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExternalEditorId {
    VisualStudioCode,
    Zed,
    SublimeText,
    Nova,
    Nano,
    Vi,
    Other,
}

impl ExternalEditorId {
    /// Stable string key used inside `pathOverrides` / on disk. Mirrors the
    /// Swift `rawValue` so stores stay interoperable.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VisualStudioCode => "visualStudioCode",
            Self::Zed => "zed",
            Self::SublimeText => "sublimeText",
            Self::Nova => "nova",
            Self::Nano => "nano",
            Self::Vi => "vi",
            Self::Other => "other",
        }
    }

    /// All variants in display order.
    pub fn all() -> &'static [ExternalEditorId] {
        &[
            Self::VisualStudioCode,
            Self::Zed,
            Self::SublimeText,
            Self::Nova,
            Self::Nano,
            Self::Vi,
            Self::Other,
        ]
    }

    /// Parse a raw key back into an id. Returns `None` on unknown values so
    /// a corrupted/partial store never crashes the resolver.
    #[allow(dead_code)]
    pub fn from_raw(raw: &str) -> Option<Self> {
        match raw {
            "visualStudioCode" => Some(Self::VisualStudioCode),
            "zed" => Some(Self::Zed),
            "sublimeText" => Some(Self::SublimeText),
            "nova" => Some(Self::Nova),
            "nano" => Some(Self::Nano),
            "vi" => Some(Self::Vi),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// Static description of a built-in editor: friendly name and the candidate
/// binary locations to probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEditorPreset {
    pub id: ExternalEditorId,
    pub display_name: &'static str,
    pub default_binary_paths: &'static [&'static str],
    pub is_terminal_editor: bool,
}

#[cfg(target_os = "macos")]
static MACOS_PRESETS: &[ExternalEditorPreset] = &[
    ExternalEditorPreset {
        id: ExternalEditorId::VisualStudioCode,
        display_name: "Visual Studio Code",
        default_binary_paths: &[
            "/opt/homebrew/bin/code",
            "/usr/local/bin/code",
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Zed,
        display_name: "Zed",
        default_binary_paths: &[
            "/opt/homebrew/bin/zedit",
            "/usr/local/bin/zedit",
            "/Applications/Zed.app/Contents/MacOS/cli/zedit",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::SublimeText,
        display_name: "Sublime Text",
        default_binary_paths: &[
            "/opt/homebrew/bin/subl",
            "/usr/local/bin/subl",
            "/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nova,
        display_name: "Nova",
        default_binary_paths: &[
            "/opt/homebrew/bin/nova",
            "/usr/local/bin/nova",
            "/Applications/Nova.app/Contents/MacOS/nova-cli",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nano,
        display_name: "nano",
        default_binary_paths: &["/usr/bin/nano", "/opt/homebrew/bin/nano"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Vi,
        display_name: "vi",
        default_binary_paths: &["/usr/bin/vi", "/usr/bin/vim"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Other,
        display_name: "Other",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
];

#[cfg(target_os = "linux")]
static LINUX_PRESETS: &[ExternalEditorPreset] = &[
    ExternalEditorPreset {
        id: ExternalEditorId::VisualStudioCode,
        display_name: "Visual Studio Code",
        default_binary_paths: &[
            "/usr/bin/code",
            "/usr/local/bin/code",
            "/snap/bin/code",
            "/var/lib/flatpak/exports/bin/com.visualstudio.code",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Zed,
        display_name: "Zed",
        default_binary_paths: &[
            "/usr/bin/zedit",
            "/usr/local/bin/zedit",
            "$HOME/.local/bin/zedit",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::SublimeText,
        display_name: "Sublime Text",
        default_binary_paths: &[
            "/usr/bin/subl",
            "/usr/local/bin/subl",
            "/opt/sublime_text/subl",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nova,
        display_name: "Nova",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nano,
        display_name: "nano",
        default_binary_paths: &["/usr/bin/nano", "/usr/local/bin/nano"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Vi,
        display_name: "vi",
        default_binary_paths: &["/usr/bin/vi", "/usr/bin/vim", "/usr/bin/nvim"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Other,
        display_name: "Other",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
];

#[cfg(target_os = "windows")]
static WINDOWS_PRESETS: &[ExternalEditorPreset] = &[
    ExternalEditorPreset {
        id: ExternalEditorId::VisualStudioCode,
        display_name: "Visual Studio Code",
        default_binary_paths: &[
            "C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd",
            "C:\\Program Files (x86)\\Microsoft VS Code\\bin\\code.cmd",
            "%LOCALAPPDATA%\\Programs\\Microsoft VS Code\\bin\\code.cmd",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Zed,
        display_name: "Zed",
        default_binary_paths: &[
            "C:\\Program Files\\Zed\\zedit.exe",
            "%LOCALAPPDATA%\\Programs\\Zed\\zedit.exe",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::SublimeText,
        display_name: "Sublime Text",
        default_binary_paths: &[
            "C:\\Program Files\\Sublime Text\\subl.exe",
            "C:\\Program Files (x86)\\Sublime Text\\subl.exe",
        ],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nova,
        display_name: "Nova",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nano,
        display_name: "nano",
        default_binary_paths: &["nano"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Vi,
        display_name: "vi",
        default_binary_paths: &["vim"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Other,
        display_name: "Other",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
];

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
static FALLBACK_PRESETS: &[ExternalEditorPreset] = &[
    ExternalEditorPreset {
        id: ExternalEditorId::VisualStudioCode,
        display_name: "Visual Studio Code",
        default_binary_paths: &["code"],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Zed,
        display_name: "Zed",
        default_binary_paths: &["zedit"],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::SublimeText,
        display_name: "Sublime Text",
        default_binary_paths: &["subl"],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nova,
        display_name: "Nova",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Nano,
        display_name: "nano",
        default_binary_paths: &["nano"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Vi,
        display_name: "vi",
        default_binary_paths: &["vi", "vim"],
        is_terminal_editor: true,
    },
    ExternalEditorPreset {
        id: ExternalEditorId::Other,
        display_name: "Other",
        default_binary_paths: &[],
        is_terminal_editor: false,
    },
];

impl ExternalEditorPreset {
    /// All built-in presets. `Other` is included with empty candidate paths
    /// (the user must supply a custom binary). Editor binary paths are
    /// platform-specific: macOS uses Homebrew + `.app` bundle paths, Linux
    /// uses `/usr/bin` + `/usr/local/bin`, and Windows uses `Program Files`.
    pub fn all() -> &'static [ExternalEditorPreset] {
        #[cfg(target_os = "macos")]
        {
            MACOS_PRESETS
        }
        #[cfg(target_os = "linux")]
        {
            LINUX_PRESETS
        }
        #[cfg(target_os = "windows")]
        {
            WINDOWS_PRESETS
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            FALLBACK_PRESETS
        }
    }

    /// Lookup a preset by id.
    pub fn for_id(id: ExternalEditorId) -> &'static Self {
        Self::all()
            .iter()
            .find(|p| p.id == id)
            .expect("preset table covers every variant")
    }

    /// First candidate path, or `""` when the preset has no built-in path
    /// (e.g. `Other`).
    pub fn default_binary_path(&self) -> &'static str {
        self.default_binary_paths.first().copied().unwrap_or("")
    }
}

/// Persisted user preferences. Fault-tolerant on load (corrupt store → default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExternalEditorPreferences {
    #[serde(default = "default_editor")]
    pub selected_editor: ExternalEditorId,
    #[serde(default)]
    pub path_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub custom_display_name: String,
    #[serde(default)]
    pub custom_binary_path: String,
}

fn default_editor() -> ExternalEditorId {
    ExternalEditorId::VisualStudioCode
}

impl Default for ExternalEditorPreferences {
    fn default() -> Self {
        Self {
            selected_editor: ExternalEditorId::VisualStudioCode,
            path_overrides: BTreeMap::new(),
            custom_display_name: String::new(),
            custom_binary_path: String::new(),
        }
    }
}

impl ExternalEditorPreferences {
    /// The binary path the resolver should try first for `id`, before probing
    /// the filesystem. `Other` always uses the custom path; everything else
    /// honors a per-id override, falling back to the preset default.
    pub fn effective_binary_path(&self, id: ExternalEditorId) -> String {
        if id == ExternalEditorId::Other {
            return self.custom_binary_path.trim().to_string();
        }
        if let Some(o) = self.path_overrides.get(id.as_str()) {
            let trimmed = o.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        ExternalEditorPreset::for_id(id)
            .default_binary_path()
            .to_string()
    }

    /// Like `effective_binary_path`, but when the chosen path does not exist
    /// (or has no candidates) falls back across the preset's candidate list.
    /// `exists` is injectable so the pure logic is unit-testable.
    pub fn resolved_binary_path(
        &self,
        id: ExternalEditorId,
        exists: impl Fn(&str) -> bool,
    ) -> String {
        let primary = self.effective_binary_path(id);
        if !primary.is_empty() && exists(&primary) {
            return primary;
        }
        // Walk the preset's candidate list, skipping the one we already tried.
        let preset = ExternalEditorPreset::for_id(id);
        for candidate in preset.default_binary_paths {
            if *candidate == primary {
                continue;
            }
            if exists(candidate) {
                return (*candidate).to_string();
            }
        }
        // Nothing on disk; return the primary (or first candidate) so the
        // caller can surface a useful "binary not found" error.
        if !primary.is_empty() {
            primary
        } else {
            preset.default_binary_path().to_string()
        }
    }

    /// Display name for `id`, honoring a custom display name when `Other`.
    #[allow(dead_code)]
    pub fn display_name(&self, id: ExternalEditorId) -> String {
        if id == ExternalEditorId::Other {
            let trimmed = self.custom_display_name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if id == ExternalEditorId::Other {
            return crate::localization::text("settings.editor.external_editors.other");
        }
        ExternalEditorPreset::for_id(id).display_name.to_string()
    }

    /// Whether `id` is a terminal editor (nano/vi).
    pub fn is_terminal_editor(&self, id: ExternalEditorId) -> bool {
        ExternalEditorPreset::for_id(id).is_terminal_editor
    }

    // Convenience accessors for the *selected* editor.
    pub fn selected_binary_path(&self) -> String {
        self.effective_binary_path(self.selected_editor)
    }
    #[allow(dead_code)]
    pub fn selected_display_name(&self) -> String {
        self.display_name(self.selected_editor)
    }
    pub fn selected_is_terminal_editor(&self) -> bool {
        self.is_terminal_editor(self.selected_editor)
    }
    pub fn selected_resolved_binary_path(&self, exists: impl Fn(&str) -> bool) -> String {
        self.resolved_binary_path(self.selected_editor, exists)
    }
}

/// File-backed store for [`ExternalEditorPreferences`]. Loads fault-tolerantly
/// (corrupt/missing file → default) and writes pretty JSON atomically.
pub struct ExternalEditorPreferencesStore;

impl ExternalEditorPreferencesStore {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load(path: &Path) -> ExternalEditorPreferences {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => ExternalEditorPreferences::default(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save(prefs: &ExternalEditorPreferences, path: &Path) -> std::io::Result<()> {
        let json = crate::config_version::to_string_pretty(prefs)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Browser build: the selection persists in localStorage (the path
    /// argument is ignored — it exists for call-site compatibility).
    #[cfg(target_arch = "wasm32")]
    pub fn load(_path: &Path) -> ExternalEditorPreferences {
        crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "external-editor.json",
        )
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn save(prefs: &ExternalEditorPreferences, _path: &Path) -> std::io::Result<()> {
        let json = crate::config_version::to_string_pretty(prefs)?;
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "external-editor.json",
            &json,
        )
        .map_err(std::io::Error::other)
    }
}

/// Default external-editor preferences file path:
/// `<data_dir>/SQLLY/external-editor.json`.
pub fn external_editor_prefs_file() -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    // Browser build: nominal path only — the store above reads/writes
    // localStorage and ignores it.
    #[cfg(target_arch = "wasm32")]
    let mut p = PathBuf::new();
    p.push("SQLLY");
    p.push("external-editor.json");
    p
}

/// Errors that can occur while launching an external editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    EmptyBinaryPath,
    #[allow(dead_code)]
    BinaryNotFound {
        path: String,
    },
    WriteFailed {
        reason: String,
    },
    LaunchFailed {
        reason: String,
    },
    /// The App Store sandbox refused to start the editor. Launches run off
    /// the UI thread, so the sentence is worded later, by `Display`.
    SandboxBlocked {
        terminal: bool,
    },
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::EmptyBinaryPath => {
                crate::localization::text("settings.editor.external_editors.error.no_binary_path")
            }
            Self::BinaryNotFound { path } => crate::localization::format(
                "settings.editor.external_editors.error.binary_not_found",
                &[("path", path)],
            ),
            Self::WriteFailed { reason } => crate::localization::format(
                "settings.editor.external_editors.error.write_failed",
                &[("reason", reason)],
            ),
            Self::LaunchFailed { reason } => crate::localization::format(
                "settings.editor.external_editors.error.launch_failed",
                &[("reason", reason)],
            ),
            Self::SandboxBlocked { terminal } => {
                let feature = if *terminal {
                    crate::localization::text(
                        "settings.editor.external_editors.error.sandbox_terminal_editor",
                    )
                } else {
                    crate::localization::text(
                        "settings.editor.external_editors.error.sandbox_command_line_editor",
                    )
                };
                crate::localization::format(
                    "settings.editor.external_editors.error.launch_failed",
                    &[(
                        "reason",
                        crate::distribution::sandbox_blocked_message(feature),
                    )],
                )
            }
        };
        f.write_str(&message)
    }
}

impl std::error::Error for LaunchError {}

#[allow(dead_code)]
/// PURE: whether the chosen editor should be fed its text over stdin rather
/// than via a temp file. Terminal editors and `.app` bundles always use a
/// temp file (they cannot consume stdin).
pub fn prefers_stdin_launch(is_terminal_editor: bool, resolved_binary_path: &str) -> bool {
    !is_terminal_editor && !resolved_binary_path.ends_with(".app")
}

#[allow(dead_code)]
/// PURE: POSIX single-quote escaping for shell arguments.
pub fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

#[allow(dead_code)]
/// PURE: escaping for a string embedded inside an AppleScript double-quoted
/// string literal.
pub fn apple_script_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// PURE: the temp file name for a tab title. Titles are text (a file name,
/// a link, a history entry), so everything but letters, digits, space, `-`,
/// `_`, and `.` becomes `_` — no path separators (the file stays inside its
/// temp directory) and no shell metacharacters (the terminal launch runs
/// through a shell). Leading dots are dropped and the length is capped.
#[allow(dead_code)]
pub fn safe_temp_file_name(name: &str) -> String {
    const MAX: usize = 120;
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_start_matches(['.', ' ']).trim_end();
    // Nothing left, or only the extension survived (`.sql`, `...sql`).
    if trimmed.is_empty() || (!trimmed.contains('.') && name.contains('.')) {
        return "query.sql".to_string();
    }
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].to_string()
}

/// PURE: the shell command a terminal runs to edit `file_path` with
/// `binary_path` — both single-quoted as whole words.
#[allow(dead_code)]
pub fn terminal_shell_command(binary_path: &str, file_path: &Path) -> String {
    format!(
        "{} {}",
        shell_quote(binary_path),
        shell_quote(&file_path.display().to_string())
    )
}

/// PURE: the AppleScript that opens Terminal.app on the edit command.
/// `do script` takes an AppleScript string literal (double-quoted).
#[allow(dead_code)]
pub fn apple_terminal_script(binary_path: &str, file_path: &Path) -> String {
    format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        apple_script_quote(&terminal_shell_command(binary_path, file_path))
    )
}

#[allow(dead_code)]
/// PURE: prune temp subdirectories under `root` older than `max_age`. Returns
/// the list of dirs that were eligible for removal (the I/O caller performs
/// the actual deletion). Injecting `now` keeps this deterministic in tests.
pub fn prune_old_temp_directories(
    root: &Path,
    max_age: Duration,
    now: SystemTime,
    entries: &[(PathBuf, SystemTime)],
) -> Vec<PathBuf> {
    let cutoff = now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH);
    entries
        .iter()
        .filter(|(p, mtime)| p.starts_with(root) && *mtime < cutoff)
        .map(|(p, _)| p.clone())
        .collect()
}

/// Write `text` to a fresh temp directory under `$TMPDIR/SQLLY-structured/`
/// and return the file path. Prunes stale sibling dirs first.
pub fn write_temp_file(text: &str, file_name: &str) -> Result<PathBuf, LaunchError> {
    // A browser tab has no temp directory (std::env::temp_dir panics on
    // wasm), and the feature exists to hand a file to a native editor.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (text, file_name);
        return Err(LaunchError::WriteFailed {
            reason: crate::browser_preview::unavailable(&crate::localization::text(
                "settings.editor.external_editors.error.temp_files_unavailable",
            )),
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    write_temp_file_native(text, file_name)
}

#[cfg(not(target_arch = "wasm32"))]
fn write_temp_file_native(text: &str, file_name: &str) -> Result<PathBuf, LaunchError> {
    let mut root = std::env::temp_dir();
    root.push("SQLLY-structured");
    prune_dir(&root, Duration::from_secs(3600), SystemTime::now());
    std::fs::create_dir_all(&root).map_err(|e| LaunchError::WriteFailed {
        reason: e.to_string(),
    })?;
    let mut dir = root;
    dir.push(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir).map_err(|e| LaunchError::WriteFailed {
        reason: e.to_string(),
    })?;
    let file_path = dir.join(safe_temp_file_name(file_name));
    std::fs::write(&file_path, text).map_err(|e| LaunchError::WriteFailed {
        reason: e.to_string(),
    })?;
    Ok(file_path)
}

fn prune_dir(root: &Path, max_age: Duration, now: SystemTime) {
    let cutoff = now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Launch `binary_path` against `file_path`. On macOS, `.app` bundles open
/// via `open`; on all platforms plain binaries spawn directly.
pub fn launch(binary_path: &str, file_path: &Path) -> Result<(), LaunchError> {
    #[cfg(target_os = "macos")]
    if binary_path.ends_with(".app") {
        std::process::Command::new("open")
            .arg(binary_path)
            .arg("--args")
            .arg(file_path)
            .status()
            .map_err(|e| LaunchError::LaunchFailed {
                reason: e.to_string(),
            })?;
        return Ok(());
    }
    // Apps open through Launch Services above; a bare executable would run
    // inside the App Store edition's sandbox and cannot be launched.
    if !crate::distribution::can_launch_user_programs() {
        return Err(LaunchError::SandboxBlocked { terminal: false });
    }
    std::process::Command::new(binary_path)
        .arg(file_path)
        .status()
        .map_err(|e| LaunchError::LaunchFailed {
            reason: e.to_string(),
        })?;
    Ok(())
}

/// Open a temp file containing `text` in the terminal editor for
/// `binary_path`. macOS uses `osascript` + Terminal.app, Linux uses
/// `x-terminal-emulator` (falling back to `xterm`), and Windows uses
/// `cmd.exe`.
pub fn launch_in_terminal(binary_path: &str, file_path: &Path) -> Result<(), LaunchError> {
    if !crate::distribution::can_launch_user_programs() {
        return Err(LaunchError::SandboxBlocked { terminal: true });
    }
    #[cfg(target_os = "macos")]
    {
        let script = apple_terminal_script(binary_path, file_path);
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .status()
            .map_err(|e| LaunchError::LaunchFailed {
                reason: e.to_string(),
            })?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let cmd = terminal_shell_command(binary_path, file_path);
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "x-terminal-emulator -e {} 2>/dev/null || xterm -e {}",
                shell_quote(&cmd),
                shell_quote(&cmd)
            ))
            .status()
            .map_err(|e| LaunchError::LaunchFailed {
                reason: e.to_string(),
            })?;
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("cmd")
            .arg("/K")
            .arg(format!("{} \"{}\"", binary_path, file_path.display()))
            .status()
            .map_err(|e| LaunchError::LaunchFailed {
                reason: e.to_string(),
            })?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        std::process::Command::new(binary_path)
            .arg(file_path)
            .status()
            .map_err(|e| LaunchError::LaunchFailed {
                reason: e.to_string(),
            })?;
        Ok(())
    }
}

/// Resolve the selected editor, write `text` to a temp file, and launch it.
pub fn open(
    text: &str,
    file_name: &str,
    prefs: &ExternalEditorPreferences,
) -> Result<PathBuf, LaunchError> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (text, file_name, prefs);
        return Err(LaunchError::LaunchFailed {
            reason: crate::browser_preview::unavailable(&crate::localization::text(
                "settings.editor.external_editors.error.browser_unavailable",
            )),
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    open_native(text, file_name, prefs)
}

#[cfg(not(target_arch = "wasm32"))]
fn open_native(
    text: &str,
    file_name: &str,
    prefs: &ExternalEditorPreferences,
) -> Result<PathBuf, LaunchError> {
    let path = prefs.selected_binary_path();
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(LaunchError::EmptyBinaryPath);
    }
    let file_path = write_temp_file(text, file_name)?;
    if prefs.selected_is_terminal_editor() {
        launch_in_terminal(trimmed, &file_path)?;
    } else {
        launch(trimmed, &file_path)?;
    }
    Ok(file_path)
}

/// Like `open`, but pipe `text` over stdin for editors that prefer it
/// (non-terminal, non-`.app`). Falls back to `open` otherwise.
#[allow(dead_code)]
pub fn open_stdin(
    text: &str,
    file_label: &str,
    prefs: &ExternalEditorPreferences,
) -> Result<(), LaunchError> {
    let path = prefs.selected_binary_path();
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(LaunchError::EmptyBinaryPath);
    }
    let resolved = trimmed.to_string();
    if !prefers_stdin_launch(prefs.selected_is_terminal_editor(), &resolved) {
        open(text, file_label, prefs)?;
        return Ok(());
    }
    if !crate::distribution::can_launch_user_programs() {
        return Err(LaunchError::SandboxBlocked { terminal: false });
    }
    use std::io::Write;
    let mut child = std::process::Command::new(&resolved)
        .arg("--stdin-filename")
        .arg(file_label)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| LaunchError::LaunchFailed {
            reason: e.to_string(),
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map_err(|e| LaunchError::LaunchFailed {
        reason: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The temp file is named after the tab: a tab title is text (from a
    /// file, a link, a history entry), so it must not steer the path out of
    /// the temp directory or carry shell metacharacters into the terminal
    /// command line.
    #[test]
    fn temp_file_name_is_a_plain_basename() {
        assert_eq!(safe_temp_file_name("Query 1.sql"), "Query 1.sql");
        assert_eq!(
            safe_temp_file_name("../../../.ssh/authorized_keys.sql"),
            "_.._.._.ssh_authorized_keys.sql"
        );
        assert_eq!(
            safe_temp_file_name("a'; rm -rf ~; echo '.sql"),
            "a__ rm -rf __ echo _.sql"
        );
        assert_eq!(safe_temp_file_name(r"C:\x&calc.sql"), "C__x_calc.sql");
        assert_eq!(safe_temp_file_name("...sql"), "query.sql");
        assert_eq!(safe_temp_file_name(""), "query.sql");
        assert!(safe_temp_file_name(&"x".repeat(500)).len() <= 120);
    }

    /// The terminal launch goes through a shell: both the editor path and
    /// the file path must be quoted as single words.
    #[test]
    fn terminal_command_quotes_binary_and_path() {
        assert_eq!(
            terminal_shell_command("/usr/bin/my vim", Path::new("/tmp/a'b; touch x")),
            "'/usr/bin/my vim' '/tmp/a'\\''b; touch x'"
        );
        // Terminal.app's `do script` takes an AppleScript string literal —
        // double-quoted, with `\` and `"` escaped.
        assert_eq!(
            apple_terminal_script("vim", Path::new("/tmp/q\"x.sql")),
            "tell application \"Terminal\"\nactivate\ndo script \"'vim' '/tmp/q\\\"x.sql'\"\nend tell"
        );
    }

    fn exists_yes(_p: &str) -> bool {
        true
    }
    fn exists_no(_p: &str) -> bool {
        false
    }
    fn exists_only(p: &str) -> bool {
        p == ExternalEditorPreset::for_id(ExternalEditorId::VisualStudioCode).default_binary_path()
    }

    fn vscode_default() -> &'static str {
        ExternalEditorPreset::for_id(ExternalEditorId::VisualStudioCode).default_binary_path()
    }

    #[test]
    fn id_round_trips_through_raw() {
        for &id in ExternalEditorId::all() {
            assert_eq!(ExternalEditorId::from_raw(id.as_str()), Some(id));
        }
        assert_eq!(ExternalEditorId::from_raw("nope"), None);
    }

    #[test]
    fn presets_cover_every_id() {
        for &id in ExternalEditorId::all() {
            assert!(ExternalEditorPreset::all().iter().any(|p| p.id == id));
        }
    }

    #[test]
    fn vscode_preset_prefers_platform_default() {
        let p = ExternalEditorPreset::for_id(ExternalEditorId::VisualStudioCode);
        assert!(!p.default_binary_path().is_empty());
        assert!(!p.is_terminal_editor);
    }

    #[test]
    fn terminal_editors_flagged() {
        assert!(ExternalEditorPreset::for_id(ExternalEditorId::Nano).is_terminal_editor);
        assert!(ExternalEditorPreset::for_id(ExternalEditorId::Vi).is_terminal_editor);
        assert!(
            !ExternalEditorPreset::for_id(ExternalEditorId::VisualStudioCode).is_terminal_editor
        );
    }

    #[test]
    fn other_preset_has_no_default_path() {
        let p = ExternalEditorPreset::for_id(ExternalEditorId::Other);
        assert_eq!(p.default_binary_path(), "");
        assert!(p.default_binary_paths.is_empty());
    }

    #[test]
    fn default_prefs_select_vscode() {
        let p = ExternalEditorPreferences::default();
        assert_eq!(p.selected_editor, ExternalEditorId::VisualStudioCode);
        assert_eq!(p.selected_binary_path(), vscode_default());
    }

    #[test]
    fn effective_path_uses_override_when_present() {
        let mut p = ExternalEditorPreferences::default();
        p.path_overrides.insert(
            "visualStudioCode".to_string(),
            "  /custom/code  ".to_string(),
        );
        assert_eq!(
            p.effective_binary_path(ExternalEditorId::VisualStudioCode),
            "/custom/code"
        );
    }

    #[test]
    fn effective_path_ignores_blank_override() {
        let mut p = ExternalEditorPreferences::default();
        p.path_overrides
            .insert("visualStudioCode".to_string(), "   ".to_string());
        assert_eq!(
            p.effective_binary_path(ExternalEditorId::VisualStudioCode),
            vscode_default()
        );
    }

    #[test]
    fn effective_path_for_other_uses_custom() {
        let p = ExternalEditorPreferences {
            custom_binary_path: "  /usr/local/bin/myedit  ".to_string(),
            ..Default::default()
        };
        assert_eq!(
            p.effective_binary_path(ExternalEditorId::Other),
            "/usr/local/bin/myedit"
        );
    }

    #[test]
    fn resolved_path_falls_back_across_candidates() {
        let p = ExternalEditorPreferences::default();
        let default = vscode_default();
        // Primary exists → returned as-is.
        assert_eq!(
            p.resolved_binary_path(ExternalEditorId::VisualStudioCode, exists_yes),
            default
        );
        // Primary missing, default present via fallback list.
        assert_eq!(
            p.resolved_binary_path(ExternalEditorId::VisualStudioCode, exists_only),
            default
        );
        // Nothing exists → returns primary anyway (caller surfaces the error).
        let resolved = p.resolved_binary_path(ExternalEditorId::VisualStudioCode, exists_no);
        assert_eq!(resolved, default);
    }

    #[test]
    fn resolved_path_honors_override_then_falls_back() {
        let mut p = ExternalEditorPreferences::default();
        p.path_overrides
            .insert("visualStudioCode".to_string(), "/missing/path".to_string());
        // Override does not exist; fallback probes the preset list.
        let resolved = p.resolved_binary_path(ExternalEditorId::VisualStudioCode, exists_only);
        assert_eq!(resolved, vscode_default());
    }

    #[test]
    fn display_name_honors_custom_for_other() {
        let p = ExternalEditorPreferences {
            custom_display_name: "  My Editor  ".to_string(),
            ..Default::default()
        };
        assert_eq!(p.display_name(ExternalEditorId::Other), "My Editor");
        assert_eq!(p.display_name(ExternalEditorId::Vi), "vi");
    }

    #[test]
    fn prefers_stdin_launch_rules() {
        // Non-terminal, plain binary → true.
        assert!(prefers_stdin_launch(false, "/opt/homebrew/bin/code"));
        // Terminal editor → false.
        assert!(!prefers_stdin_launch(true, "/usr/bin/nano"));
        // .app bundle → false even when non-terminal.
        assert!(!prefers_stdin_launch(false, "/Applications/Zed.app"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("simple"), "'simple'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a'b'c"), "'a'\\''b'\\''c'");
    }

    #[test]
    fn apple_script_quote_escapes_backslash_and_double_quote() {
        assert_eq!(apple_script_quote("plain"), "plain");
        assert_eq!(apple_script_quote("a\\b"), "a\\\\b");
        assert_eq!(apple_script_quote("say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn prune_drops_old_keeps_recent() {
        let root = Path::new("/tmp/SQLLY-structured");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let old = now - Duration::from_secs(7200); // 2h ago, past 1h cutoff
        let recent = now - Duration::from_secs(60);
        let entries = vec![
            (root.join("a"), old),
            (root.join("b"), recent),
            (root.join("c"), now),
        ];
        let to_drop = prune_old_temp_directories(root, Duration::from_secs(3600), now, &entries);
        assert_eq!(to_drop, vec![root.join("a")]);
    }

    #[test]
    fn prune_ignores_paths_outside_root() {
        let root = Path::new("/tmp/SQLLY-structured");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let old = now - Duration::from_secs(7200);
        let entries = vec![
            (Path::new("/tmp/other/a").to_path_buf(), old),
            (root.join("a"), old),
        ];
        let to_drop = prune_old_temp_directories(root, Duration::from_secs(3600), now, &entries);
        assert_eq!(to_drop, vec![root.join("a")]);
    }

    #[test]
    fn launch_error_display_is_human_readable() {
        assert_eq!(
            LaunchError::EmptyBinaryPath.to_string(),
            "No editor binary path is set"
        );
        assert_eq!(
            LaunchError::BinaryNotFound {
                path: "/x".to_string()
            }
            .to_string(),
            "Editor binary not found: /x"
        );
    }

    #[test]
    fn preferences_round_trip_through_json() {
        let mut p = ExternalEditorPreferences::default();
        p.path_overrides
            .insert("zed".to_string(), "/custom/zed".to_string());
        p.custom_display_name = "Zed Dev".to_string();
        p.custom_binary_path = "/custom/zed".to_string();
        let json = serde_json::to_string(&p).unwrap();
        let back: ExternalEditorPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn preferences_load_defaults_on_corrupt_json() {
        let tmp = std::env::temp_dir().join("sqlly-ext-editor-test-corrupt.json");
        std::fs::write(&tmp, "{ not json").unwrap();
        let loaded = ExternalEditorPreferencesStore::load(&tmp);
        assert_eq!(loaded, ExternalEditorPreferences::default());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn preferences_store_save_load_round_trip() {
        let tmp = std::env::temp_dir().join("sqlly-ext-editor-test-rt.json");
        let p = ExternalEditorPreferences {
            custom_display_name: "Custom".to_string(),
            ..Default::default()
        };
        ExternalEditorPreferencesStore::save(&p, &tmp).unwrap();
        let back = ExternalEditorPreferencesStore::load(&tmp);
        assert_eq!(p, back);
        let _ = std::fs::remove_file(&tmp);
    }
}
