//! The embedded-output commands' gpui actions, rebindable shortcut ids,
//! default keys and palette table, wired like the editing commands.

use std::collections::HashMap;

use gpui::{App, Context, Div, InteractiveElement as _, Stateful, Window};

use super::EmbeddedCommand;
use crate::keybindings::{KeyBinding, KeyStroke, Modifiers};
use crate::notebook::view::NotebookView;

gpui::actions!(
    notebook_embedded,
    [ToggleEmbedded, EmbedAllOutputs, ToggleEmbeddedConfig]
);

/// One command: its shortcut/palette id, title key and palette keywords.
pub struct CommandSpec {
    pub id: &'static str,
    pub command: EmbeddedCommand,
    pub title_key: &'static str,
    pub keywords: &'static [&'static str],
}

/// Every embedded-output command, in palette order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: "notebook.toggleEmbedded",
        command: EmbeddedCommand::Toggle,
        title_key: "notebook.embedded.palette.toggle",
        keywords: &["notebook", "embed", "embedded", "output", "full", "report"],
    },
    CommandSpec {
        id: "notebook.embedAllOutputs",
        command: EmbeddedCommand::EmbedAll,
        title_key: "notebook.embedded.palette.embed_all",
        keywords: &["notebook", "embed", "all", "outputs", "report", "document"],
    },
    CommandSpec {
        id: "notebook.toggleEmbeddedConfig",
        command: EmbeddedCommand::ToggleConfig,
        title_key: "notebook.embedded.palette.toggle_config",
        keywords: &[
            "notebook",
            "embed",
            "pivot",
            "chart",
            "configuration",
            "sidebar",
        ],
    },
];

/// The command a shortcut/palette id names.
pub fn command_for(id: &str) -> Option<EmbeddedCommand> {
    COMMANDS.iter().find(|c| c.id == id).map(|c| c.command)
}

/// Default keys: E in command mode toggles the selected cells.
pub fn insert_defaults(m: &mut HashMap<String, Vec<KeyBinding>>) {
    m.insert(
        "notebook.toggleEmbedded".to_string(),
        vec![KeyBinding {
            strokes: vec![KeyStroke::new('e', Modifiers::none())],
            when: Some(crate::notebook::COMMAND_KEY_CONTEXT.to_string()),
        }],
    );
    m.insert("notebook.embedAllOutputs".to_string(), Vec::new());
    m.insert("notebook.toggleEmbeddedConfig".to_string(), Vec::new());
}

/// Load `stroke` for command `id`; `None` when `id` is not one of these.
pub fn load(id: &str, stroke: &str, when: Option<&str>, cx: &App) -> Option<gpui::KeyBinding> {
    use crate::keybindings::load_binding as l;
    match command_for(id)? {
        EmbeddedCommand::Toggle => l(stroke, ToggleEmbedded, when, cx),
        EmbeddedCommand::EmbedAll => l(stroke, EmbedAllOutputs, when, cx),
        EmbeddedCommand::ToggleConfig => l(stroke, ToggleEmbeddedConfig, when, cx),
    }
}

/// The action handlers on the notebook's root element.
pub(crate) fn register_actions(
    root: Stateful<Div>,
    cx: &mut Context<NotebookView>,
) -> Stateful<Div> {
    root.on_action(cx.listener(
        |this: &mut NotebookView, _: &ToggleEmbedded, _: &mut Window, cx| {
            this.embedded_command(EmbeddedCommand::Toggle, cx)
        },
    ))
    .on_action(cx.listener(
        |this: &mut NotebookView, _: &EmbedAllOutputs, _: &mut Window, cx| {
            this.embedded_command(EmbeddedCommand::EmbedAll, cx)
        },
    ))
    .on_action(cx.listener(
        |this: &mut NotebookView, _: &ToggleEmbeddedConfig, _: &mut Window, cx| {
            this.embedded_command(EmbeddedCommand::ToggleConfig, cx)
        },
    ))
}
