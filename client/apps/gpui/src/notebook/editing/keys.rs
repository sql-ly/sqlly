//! The editing commands' gpui actions, their rebindable shortcut ids and
//! default keys, and the palette table. `keybindings.rs` asks this module
//! for the defaults and to load a stroke for one of these ids.

use std::collections::HashMap;

use gpui::{App, Context, Div, InteractiveElement as _, Stateful, Window};

use super::EditingCommand;
use crate::keybindings::{KeyBinding, KeyStroke, Modifiers};
use crate::notebook::view::NotebookView;

gpui::actions!(
    notebook_editing,
    [
        SelectAllCells,
        ExtendSelectionDown,
        ExtendSelectionUp,
        ClearSelection,
        CopyCells,
        CutCells,
        PasteCellsBelow,
        PasteCellsAbove,
        DuplicateCells,
        UndoStructure,
        RedoStructure,
        MoveCellsUp,
        MoveCellsDown,
        ToggleCollapseCells,
        RunSelectedCells,
        SplitCell,
        MergeCellBelow,
        MergeCellAbove,
        OpenInsertMenu,
        FindInNotebook,
        ReplaceInNotebook,
        ToggleBookmark,
        NextBookmark,
        PreviousBookmark,
        ToggleFoldSection,
        UnfoldAll,
        TogglePinOutput,
        ShowBookmarks,
    ]
);

const UP: char = '\u{F700}';
const DOWN: char = '\u{F701}';
const ESC: char = '\u{1B}';

/// One editing command: its shortcut/palette id, title key and palette
/// keywords.
pub struct CommandSpec {
    pub id: &'static str,
    pub command: EditingCommand,
    pub title_key: &'static str,
    pub keywords: &'static [&'static str],
}

macro_rules! spec {
    ($id:literal, $cmd:ident, $title:literal, [$($kw:literal),*]) => {
        CommandSpec {
            id: $id,
            command: EditingCommand::$cmd,
            title_key: $title,
            keywords: &["notebook", $($kw),*],
        }
    };
}

/// Every editing command, in palette order.
pub const COMMANDS: &[CommandSpec] = &[
    spec!(
        "notebook.selectAllCells",
        SelectAll,
        "notebook.editing.palette.select_all",
        ["select", "all", "cells"]
    ),
    spec!(
        "notebook.extendSelectionDown",
        ExtendDown,
        "notebook.editing.palette.extend_down",
        ["select", "extend", "down", "shift"]
    ),
    spec!(
        "notebook.extendSelectionUp",
        ExtendUp,
        "notebook.editing.palette.extend_up",
        ["select", "extend", "up", "shift"]
    ),
    spec!(
        "notebook.clearSelection",
        ClearSelection,
        "notebook.editing.palette.clear_selection",
        ["select", "clear", "deselect"]
    ),
    spec!(
        "notebook.copyCells",
        Copy,
        "notebook.editing.palette.copy",
        ["copy", "clipboard", "cells"]
    ),
    spec!(
        "notebook.cutCells",
        Cut,
        "notebook.editing.palette.cut",
        ["cut", "clipboard", "cells"]
    ),
    spec!(
        "notebook.pasteCells",
        PasteBelow,
        "notebook.editing.palette.paste_below",
        ["paste", "clipboard", "below"]
    ),
    spec!(
        "notebook.pasteCellsAbove",
        PasteAbove,
        "notebook.editing.palette.paste_above",
        ["paste", "clipboard", "above"]
    ),
    spec!(
        "notebook.duplicateCell",
        Duplicate,
        "notebook.editing.palette.duplicate",
        ["duplicate", "copy", "clone"]
    ),
    spec!(
        "notebook.undoStructure",
        Undo,
        "notebook.editing.palette.undo",
        ["undo", "structure"]
    ),
    spec!(
        "notebook.redoStructure",
        Redo,
        "notebook.editing.palette.redo",
        ["redo", "structure"]
    ),
    spec!(
        "notebook.moveCellUp",
        MoveUp,
        "notebook.editing.palette.move_up",
        ["move", "up", "reorder"]
    ),
    spec!(
        "notebook.moveCellDown",
        MoveDown,
        "notebook.editing.palette.move_down",
        ["move", "down", "reorder"]
    ),
    spec!(
        "notebook.toggleCollapse",
        ToggleCollapse,
        "notebook.editing.palette.toggle_collapse",
        ["collapse", "expand", "hide"]
    ),
    spec!(
        "notebook.runSelected",
        RunSelected,
        "notebook.editing.palette.run_selected",
        ["run", "selected", "cells"]
    ),
    spec!(
        "notebook.splitCell",
        Split,
        "notebook.editing.palette.split",
        ["split", "cursor", "cell"]
    ),
    spec!(
        "notebook.mergeCellBelow",
        MergeBelow,
        "notebook.editing.palette.merge_below",
        ["merge", "join", "below"]
    ),
    spec!(
        "notebook.mergeCellAbove",
        MergeAbove,
        "notebook.editing.palette.merge_above",
        ["merge", "join", "above"]
    ),
    spec!(
        "notebook.insertCellMenu",
        InsertMenu,
        "notebook.editing.palette.insert_menu",
        ["insert", "add", "cell", "kind"]
    ),
    spec!(
        "notebook.findInNotebook",
        Find,
        "notebook.editing.palette.find",
        ["find", "search"]
    ),
    spec!(
        "notebook.replaceInNotebook",
        Replace,
        "notebook.editing.palette.replace",
        ["replace", "find", "search"]
    ),
    spec!(
        "notebook.toggleBookmark",
        ToggleBookmark,
        "notebook.editing.palette.toggle_bookmark",
        ["bookmark", "mark"]
    ),
    spec!(
        "notebook.nextBookmark",
        NextBookmark,
        "notebook.editing.palette.next_bookmark",
        ["bookmark", "next"]
    ),
    spec!(
        "notebook.previousBookmark",
        PreviousBookmark,
        "notebook.editing.palette.previous_bookmark",
        ["bookmark", "previous"]
    ),
    spec!(
        "notebook.showBookmarks",
        ShowBookmarks,
        "notebook.editing.palette.show_bookmarks",
        ["bookmarks", "rail", "list"]
    ),
    spec!(
        "notebook.toggleFoldSection",
        ToggleFold,
        "notebook.editing.palette.toggle_fold",
        ["fold", "heading", "section"]
    ),
    spec!(
        "notebook.unfoldAll",
        UnfoldAll,
        "notebook.editing.palette.unfold_all",
        ["unfold", "expand", "sections"]
    ),
    spec!(
        "notebook.togglePinOutput",
        TogglePin,
        "notebook.editing.palette.toggle_pin",
        ["pin", "output", "sticky"]
    ),
];

/// The command a shortcut/palette id names.
pub fn command_for(id: &str) -> Option<EditingCommand> {
    COMMANDS.iter().find(|c| c.id == id).map(|c| c.command)
}

/// Default keys. Single letters and plain arrows fire only in command mode;
/// split works inside a cell editor (it needs the cursor).
pub fn insert_defaults(m: &mut HashMap<String, Vec<KeyBinding>>) {
    let command_mode = |stroke: KeyStroke| KeyBinding {
        strokes: vec![stroke],
        when: Some(crate::notebook::COMMAND_KEY_CONTEXT.to_string()),
    };
    let in_cell_editor = |stroke: KeyStroke| KeyBinding {
        strokes: vec![stroke],
        when: Some(format!("{} > SqlEditor", crate::notebook::KEY_CONTEXT)),
    };
    let key = |k: char| KeyStroke::new(k, Modifiers::none());
    let shift = |k: char| KeyStroke::new(k, Modifiers::shift());
    let cmd = |k: char| KeyStroke::new(k, Modifiers::command());
    let cmd_shift = |k: char| KeyStroke::new(k, Modifiers::cmd_shift());
    let option = |k: char| {
        KeyStroke::new(
            k,
            Modifiers {
                option: true,
                ..Modifiers::none()
            },
        )
    };
    let shift_f2 = KeyStroke {
        modifiers: Modifiers::shift(),
        ..KeyStroke::function_key(2)
    };
    let cmd_f2 = KeyStroke {
        modifiers: Modifiers::command(),
        ..KeyStroke::function_key(2)
    };
    let defaults: Vec<(&str, Vec<KeyBinding>)> = vec![
        ("notebook.selectAllCells", vec![command_mode(cmd('a'))]),
        (
            "notebook.extendSelectionDown",
            vec![command_mode(shift('j')), command_mode(shift(DOWN))],
        ),
        (
            "notebook.extendSelectionUp",
            vec![command_mode(shift('k')), command_mode(shift(UP))],
        ),
        ("notebook.clearSelection", vec![command_mode(key(ESC))]),
        (
            "notebook.copyCells",
            vec![command_mode(key('c')), command_mode(cmd('c'))],
        ),
        (
            "notebook.cutCells",
            vec![command_mode(key('x')), command_mode(cmd('x'))],
        ),
        (
            "notebook.pasteCells",
            vec![command_mode(key('v')), command_mode(cmd('v'))],
        ),
        ("notebook.pasteCellsAbove", vec![command_mode(shift('v'))]),
        ("notebook.duplicateCell", vec![command_mode(cmd('d'))]),
        (
            "notebook.undoStructure",
            vec![command_mode(key('z')), command_mode(cmd('z'))],
        ),
        (
            "notebook.redoStructure",
            vec![command_mode(shift('z')), command_mode(cmd_shift('z'))],
        ),
        ("notebook.moveCellUp", vec![command_mode(option(UP))]),
        ("notebook.moveCellDown", vec![command_mode(option(DOWN))]),
        ("notebook.toggleCollapse", vec![command_mode(key('o'))]),
        (
            "notebook.runSelected",
            vec![command_mode(KeyStroke::new('\r', Modifiers::cmd_option()))],
        ),
        ("notebook.splitCell", vec![in_cell_editor(cmd_shift('-'))]),
        ("notebook.mergeCellBelow", vec![command_mode(shift('m'))]),
        ("notebook.mergeCellAbove", Vec::new()),
        ("notebook.insertCellMenu", vec![command_mode(key('i'))]),
        ("notebook.findInNotebook", vec![command_mode(cmd('f'))]),
        (
            "notebook.replaceInNotebook",
            vec![command_mode(cmd_shift('h'))],
        ),
        ("notebook.toggleBookmark", vec![command_mode(cmd_f2)]),
        (
            "notebook.nextBookmark",
            vec![command_mode(KeyStroke::function_key(2))],
        ),
        ("notebook.previousBookmark", vec![command_mode(shift_f2)]),
        ("notebook.showBookmarks", Vec::new()),
        (
            "notebook.toggleFoldSection",
            vec![command_mode(KeyStroke::new('[', Modifiers::cmd_option()))],
        ),
        ("notebook.unfoldAll", Vec::new()),
        ("notebook.togglePinOutput", vec![command_mode(key('p'))]),
    ];
    for (id, bindings) in defaults {
        m.insert(id.to_string(), bindings);
    }
}

/// Load `stroke` for editing command `id`; `None` when `id` is not one
/// (or the stroke does not parse).
pub fn load(id: &str, stroke: &str, when: Option<&str>, cx: &App) -> Option<gpui::KeyBinding> {
    use crate::keybindings::load_binding as l;
    match command_for(id)? {
        EditingCommand::SelectAll => l(stroke, SelectAllCells, when, cx),
        EditingCommand::ExtendDown => l(stroke, ExtendSelectionDown, when, cx),
        EditingCommand::ExtendUp => l(stroke, ExtendSelectionUp, when, cx),
        EditingCommand::ClearSelection => l(stroke, ClearSelection, when, cx),
        EditingCommand::Copy => l(stroke, CopyCells, when, cx),
        EditingCommand::Cut => l(stroke, CutCells, when, cx),
        EditingCommand::PasteBelow => l(stroke, PasteCellsBelow, when, cx),
        EditingCommand::PasteAbove => l(stroke, PasteCellsAbove, when, cx),
        EditingCommand::Duplicate => l(stroke, DuplicateCells, when, cx),
        EditingCommand::Undo => l(stroke, UndoStructure, when, cx),
        EditingCommand::Redo => l(stroke, RedoStructure, when, cx),
        EditingCommand::MoveUp => l(stroke, MoveCellsUp, when, cx),
        EditingCommand::MoveDown => l(stroke, MoveCellsDown, when, cx),
        EditingCommand::ToggleCollapse => l(stroke, ToggleCollapseCells, when, cx),
        EditingCommand::RunSelected => l(stroke, RunSelectedCells, when, cx),
        EditingCommand::Split => l(stroke, SplitCell, when, cx),
        EditingCommand::MergeBelow => l(stroke, MergeCellBelow, when, cx),
        EditingCommand::MergeAbove => l(stroke, MergeCellAbove, when, cx),
        EditingCommand::InsertMenu => l(stroke, OpenInsertMenu, when, cx),
        EditingCommand::Find => l(stroke, FindInNotebook, when, cx),
        EditingCommand::Replace => l(stroke, ReplaceInNotebook, when, cx),
        EditingCommand::ToggleBookmark => l(stroke, ToggleBookmark, when, cx),
        EditingCommand::NextBookmark => l(stroke, NextBookmark, when, cx),
        EditingCommand::PreviousBookmark => l(stroke, PreviousBookmark, when, cx),
        EditingCommand::ShowBookmarks => l(stroke, ShowBookmarks, when, cx),
        EditingCommand::ToggleFold => l(stroke, ToggleFoldSection, when, cx),
        EditingCommand::UnfoldAll => l(stroke, UnfoldAll, when, cx),
        EditingCommand::TogglePin => l(stroke, TogglePinOutput, when, cx),
    }
}

/// The action handlers on the notebook's root element.
pub(crate) fn register_actions(
    root: Stateful<Div>,
    cx: &mut Context<NotebookView>,
) -> Stateful<Div> {
    macro_rules! on {
        ($root:expr, $action:ty, $cmd:ident) => {
            $root.on_action(cx.listener(
                |this: &mut NotebookView, _: &$action, window: &mut Window, cx| {
                    this.editing_command_in(EditingCommand::$cmd, Some(window), cx)
                },
            ))
        };
    }
    let root = on!(root, SelectAllCells, SelectAll);
    let root = on!(root, ExtendSelectionDown, ExtendDown);
    let root = on!(root, ExtendSelectionUp, ExtendUp);
    let root = on!(root, ClearSelection, ClearSelection);
    let root = on!(root, CopyCells, Copy);
    let root = on!(root, CutCells, Cut);
    let root = on!(root, PasteCellsBelow, PasteBelow);
    let root = on!(root, PasteCellsAbove, PasteAbove);
    let root = on!(root, DuplicateCells, Duplicate);
    let root = on!(root, UndoStructure, Undo);
    let root = on!(root, RedoStructure, Redo);
    let root = on!(root, MoveCellsUp, MoveUp);
    let root = on!(root, MoveCellsDown, MoveDown);
    let root = on!(root, ToggleCollapseCells, ToggleCollapse);
    let root = on!(root, RunSelectedCells, RunSelected);
    let root = on!(root, SplitCell, Split);
    let root = on!(root, MergeCellBelow, MergeBelow);
    let root = on!(root, MergeCellAbove, MergeAbove);
    let root = on!(root, OpenInsertMenu, InsertMenu);
    let root = on!(root, FindInNotebook, Find);
    let root = on!(root, ReplaceInNotebook, Replace);
    let root = on!(root, ToggleBookmark, ToggleBookmark);
    let root = on!(root, NextBookmark, NextBookmark);
    let root = on!(root, PreviousBookmark, PreviousBookmark);
    let root = on!(root, ShowBookmarks, ShowBookmarks);
    let root = on!(root, ToggleFoldSection, ToggleFold);
    let root = on!(root, UnfoldAll, UnfoldAll);
    on!(root, TogglePinOutput, TogglePin)
}
