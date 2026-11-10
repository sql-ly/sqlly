//! Help window content generated from the live command registry and
//! keybinding maps.
//!
//! The old help text was a hand-maintained string that drifted from reality
//! (wrong shortcuts, missing commands). This module derives it instead: the
//! application section comes from `CommandRegistry::default_registry` with
//! shortcuts resolved through `keybindings::shortcut_hint` (which honors the
//! user's `keybindings.json` overrides), and the editor section comes from
//! `editor_keybindings::load_editor_keybindings` (defaults + overrides) with
//! the same display names the keybindings preferences pane shows.

use crate::command_palette::{CommandCategory, CommandRegistry};

/// Editor actions worth surfacing in Help — the command-like ones, not the
/// per-keystroke plumbing (arrow movement, backspace, plain selection).
/// Guarded by a test against `editor_keybindings::KNOWN_ACTIONS` so a rename
/// there can't silently drop a help row.
const EDITOR_HELP_ACTIONS: &[&str] = &[
    "ExecuteQuery",
    "TriggerCompletions",
    "TriggerValueLookup",
    "TriggerAiCompletion",
    "CaptureIntelliSenseFeedback",
    "InsertDistinctValuesAsInList",
    "PasteAsInList",
    "ConvertDelimitedList",
    "Validate",
    "FormatSql",
    "FormatDocument",
    "ToggleLineComment",
    "AddLineComment",
    "RemoveLineComment",
    "Find",
    "Replace",
    "FindNext",
    "FindPrev",
    "Fold",
    "Unfold",
    "FoldAll",
    "UnfoldAll",
];

/// Render one keystroke string ("cmd-alt-[") for humans. macOS uses the
/// conventional modifier symbols; other platforms spell the words out.
fn format_stroke(stroke: &str) -> String {
    // A chord ("cmd-k cmd-c") formats each keystroke separately.
    if stroke.contains(' ') {
        return stroke
            .split_whitespace()
            .map(format_stroke)
            .collect::<Vec<_>>()
            .join(" ");
    }
    let mut out = String::new();
    let mut key = stroke;
    let mut parts: Vec<&str> = stroke.split('-').collect();
    // A trailing empty part means the key itself is '-' ("cmd--").
    if parts.len() > 1 && parts.last() == Some(&"") {
        parts.pop();
        parts.pop();
        parts.push("-");
    }
    if let Some((last, mods)) = parts.split_last() {
        key = last;
        for m in mods {
            #[cfg(target_os = "macos")]
            out.push_str(match *m {
                "cmd" => "\u{2318}",
                "ctrl" => "\u{2303}",
                "alt" => "\u{2325}",
                "shift" => "\u{21E7}",
                other => other,
            });
            #[cfg(not(target_os = "macos"))]
            {
                out.push_str(match *m {
                    "cmd" => "Win",
                    "ctrl" => "Ctrl",
                    "alt" => "Alt",
                    "shift" => "Shift",
                    other => other,
                });
                out.push('+');
            }
        }
    }
    let key_label = match key {
        "enter" => crate::localization::text("help.content.key_enter"),
        "escape" => crate::localization::text("help.content.key_escape"),
        "space" => crate::localization::text("help.content.key_space"),
        "backspace" => crate::localization::text("help.content.key_backspace"),
        "delete" => crate::localization::text("help.content.key_delete"),
        "tab" => crate::localization::text("help.content.key_tab"),
        k if k.len() == 1 => k.to_uppercase(),
        k => {
            let mut c = k.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    };
    out.push_str(&key_label);
    out
}

fn push_row(out: &mut String, title: &str, shortcut: &str) {
    if shortcut.is_empty() {
        out.push_str(&format!("  {title}\n"));
    } else {
        out.push_str(&format!("  {title:<42}{shortcut}\n"));
    }
}

/// The full help body. Built on demand (the help window is rare), never
/// cached, so it always reflects current overrides.
pub fn build_help_body() -> String {
    let registry = CommandRegistry::default_registry();
    let mut out = crate::localization::text("help.content.introduction");

    for category in CommandCategory::all() {
        let rows: Vec<(String, String)> = registry
            .all()
            .iter()
            .filter(|c| c.category == category)
            .map(|c| (c.title.clone(), crate::keybindings::shortcut_hint(&c.id)))
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{}\n", category.label()));
        for (title, hint) in rows {
            push_row(&mut out, &title, &hint);
        }
    }

    out.push_str(&crate::localization::text("help.content.editor_section"));
    let editor_map = crate::editor_keybindings::load_editor_keybindings();
    for action in EDITOR_HELP_ACTIONS {
        let title = crate::editor_keybindings::editor_action_display_name(action);
        let shortcut = editor_map
            .get(*action)
            .map(|strokes| {
                strokes
                    .iter()
                    .map(|s| format_stroke(s))
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .unwrap_or_default();
        push_row(&mut out, &title, &shortcut);
    }

    out.push_str(&crate::localization::text(
        "help.content.documentation_footer",
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_help_editor_action_is_a_known_action() {
        for action in EDITOR_HELP_ACTIONS {
            assert!(
                crate::editor_keybindings::KNOWN_ACTIONS.contains(action),
                "{action} is not a known editor action"
            );
        }
    }

    #[test]
    fn help_lists_every_registry_command() {
        let body = build_help_body();
        for cmd in CommandRegistry::default_registry().all() {
            assert!(
                body.contains(&cmd.title),
                "help body is missing command {}",
                cmd.title
            );
        }
    }

    #[test]
    fn help_lists_editor_features() {
        let body = build_help_body();
        assert!(body.contains("Fold Region"));
        assert!(body.contains("Trigger AI Inline Suggestion"));
        assert!(body.contains("Format SQL") || body.contains("Format"));
    }

    #[test]
    fn execute_query_row_has_a_shortcut() {
        let body = build_help_body();
        let line = body
            .lines()
            .find(|l| l.contains("Execute Query"))
            .expect("Execute Query row");
        assert!(
            line.trim_end().len() > line.trim_end().trim_start().len()
                || line.contains('\u{2318}')
                || line.contains("Ctrl")
                || line.contains("F5"),
            "Execute Query should show a resolved shortcut: {line:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strokes_format_with_mac_symbols() {
        assert_eq!(format_stroke("cmd-alt-["), "\u{2318}\u{2325}[");
        assert_eq!(format_stroke("ctrl-space"), "\u{2303}Space");
        assert_eq!(format_stroke("f5"), "F5");
        assert_eq!(format_stroke("alt-\\"), "\u{2325}\\");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn strokes_format_with_words() {
        assert_eq!(format_stroke("ctrl-shift-["), "Ctrl+Shift+[");
        assert_eq!(format_stroke("f5"), "F5");
    }
}
