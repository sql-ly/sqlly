//! Windowed tests for notebook editing: selection and bulk actions, the
//! clipboard, structural undo, moves, split and merge, the insert menu,
//! find and replace, bookmarks, folds, pins and reference IntelliSense.

use gpui::{
    AppContext as _, ClipboardItem, Entity, Focusable as _, ParentElement as _, Styled as _,
    TestAppContext, VisualTestContext, px, size,
};
use sqlly_services::notebook::editing::Fragment;
use sqlly_services::notebook::{Cell, CellKind, ExecutionPolicy, NotebookDoc};

use super::{BOOKMARK_KEY, EditingCommand, FOLDED_KEY, PINNED_KEY};
use crate::editor::SqlEditor;
use crate::notebook::view::{NotebookOpening, NotebookView};
use crate::results::ResultsView;

/// Points autosave at a temp dir for the test's lifetime.
pub(super) struct AutosaveRoot(#[allow(dead_code)] tempfile::TempDir);

impl AutosaveRoot {
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self(dir)
    }
}

impl Drop for AutosaveRoot {
    fn drop(&mut self) {
        crate::notebook::autosave::set_test_root(None);
    }
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        crate::editor::register_keybindings(cx);
        crate::keybindings::register_app_keybindings_from(
            &crate::keybindings::KeybindingSettings::default(),
            cx,
        );
    });
}

pub(super) fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    init(cx);
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| {
            NotebookView::with_choices(model, NotebookOpening::New, std::rc::Rc::new(Vec::new), cx)
        });
        gpui_component::Root::new(nb, window, cx)
    });
    let view = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .expect("root hosts the notebook")
    });
    (view, vcx)
}

pub(super) fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        size(px(1400.0), px(900.0)),
        move |_, _| gpui::div().size_full().child(view.clone()),
    );
}

fn command_mode(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    draw(view, cx);
    cx.update(|window, cx| window.focus(&view.read(cx).focus_handle(cx), cx));
    cx.run_until_parked();
}

/// Act as the host: answer every editor request with plain entities.
fn serve_entities(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let target = view.downgrade();
    cx.update(|_, cx| {
        cx.subscribe(
            view,
            move |_, event: &crate::notebook::view::NotebookEvent, cx| {
                if let crate::notebook::view::NotebookEvent::NeedCellEntities { cell_ids } = event {
                    let ids = cell_ids.clone();
                    let target = target.clone();
                    cx.defer(move |cx| {
                        let pairs: Vec<_> = ids
                            .into_iter()
                            .map(|id| (id, cx.new(SqlEditor::new), cx.new(ResultsView::new)))
                            .collect();
                        target
                            .update(cx, |nb, cx| nb.attach_cell_entities(pairs, cx))
                            .ok();
                    });
                }
            },
        )
        .detach();
    });
}

fn cell(nb: &mut NotebookDoc, kind: CellKind, name: Option<&str>, source: &str) {
    let id = format!("cell_{}", nb.cells.len() + 1);
    let mut cell = Cell::new(id, kind, ExecutionPolicy::default());
    cell.name = name.map(str::to_string);
    cell.source = source.to_string();
    nb.cells.push(cell);
}

fn doc() -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nbedit", "Revenue review");
    cell(
        &mut nb,
        CellKind::Markdown,
        None,
        "# Revenue\nWhy revenue fell.",
    );
    cell(
        &mut nb,
        CellKind::Sql,
        Some("orders"),
        "SELECT order_id, amount FROM sales.orders",
    );
    cell(
        &mut nb,
        CellKind::Sql,
        Some("totals"),
        "SELECT sum(amount) AS revenue FROM {{orders}}",
    );
    cell(&mut nb, CellKind::Markdown, None, "## Detail");
    cell(&mut nb, CellKind::Sql, None, "SELECT 1");
    nb
}

fn ids(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> Vec<String> {
    view.read_with(cx, |nb, _| {
        nb.doc().cells().map(|c| c.id.to_string()).collect()
    })
}

fn sources(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> Vec<String> {
    view.read_with(cx, |nb, _| {
        nb.doc().cells().map(|c| c.source.to_string()).collect()
    })
}

#[gpui::test]
fn shift_keys_select_and_bulk_delete_is_one_undo_step(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let before = ids(&view, cx);
    command_mode(&view, cx);
    cx.simulate_keystrokes("j shift-j shift-j");
    let selected = view.read_with(cx, |nb, _| nb.selection());
    assert_eq!(selected, before[1..4].to_vec());
    draw(&view, cx);
    cx.simulate_keystrokes("d d");
    assert_eq!(ids(&view, cx), vec![before[0].clone(), before[4].clone()]);
    assert!(
        view.read_with(cx, |nb, _| nb.editing.toast.is_some()),
        "delete shows the undo toast"
    );
    cx.simulate_keystrokes("z");
    assert_eq!(
        ids(&view, cx),
        before,
        "one undo brings every cell back in place"
    );
    cx.simulate_keystrokes("shift-z");
    assert_eq!(ids(&view, cx).len(), 2);
    view.update(cx, |nb, cx| nb.undo_delete(cx));
    assert_eq!(ids(&view, cx), before, "the toast's Undo is the same step");
}

#[gpui::test]
fn select_all_and_escape(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    command_mode(&view, cx);
    cx.simulate_keystrokes("secondary-a");
    assert_eq!(view.read_with(cx, |nb, _| nb.selection().len()), 5);
    draw(&view, cx);
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |nb, _| nb.selected.len()), 0);
}

#[gpui::test]
fn shift_click_selects_a_range(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| {
        nb.click_cell(&all[1], gpui::Modifiers::default(), cx);
        nb.click_cell(
            &all[3],
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            cx,
        );
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.selection()),
        all[1..4].to_vec()
    );
    view.update(cx, |nb, cx| {
        nb.click_cell(&all[0], gpui::Modifiers::default(), cx)
    });
    assert!(view.read_with(cx, |nb, _| nb.selected.is_empty()));
}

#[gpui::test]
fn copy_paste_regenerates_ids_dedupes_names_and_rewrites_references(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    command_mode(&view, cx);
    view.update(cx, |nb, cx| {
        nb.selected = vec![all[1].clone(), all[2].clone()];
        nb.activate_cell(&all[2], cx);
    });
    cx.simulate_keystrokes("c");
    let item = cx.update(|_, cx| cx.read_from_clipboard()).expect("copied");
    assert_eq!(
        item.text().unwrap(),
        "SELECT order_id, amount FROM sales.orders\nGO\n\nSELECT sum(amount) AS revenue FROM {{orders}}\nGO\n",
        "SQL cells copy as a script other apps can paste"
    );
    assert!(Fragment::from_json(item.metadata().unwrap()).is_some());
    view.update(cx, |nb, cx| nb.activate_cell(&all[4], cx));
    draw(&view, cx);
    cx.simulate_keystrokes("v");
    let after = ids(&view, cx);
    assert_eq!(after.len(), 7);
    let pasted = &after[5..];
    assert!(
        pasted.iter().all(|id| !all.contains(id)),
        "pasted cells get new ids"
    );
    let (names, sql) = view.read_with(cx, |nb, _| {
        let m = nb.model();
        (
            [m.cells[5].name.clone(), m.cells[6].name.clone()],
            m.cells[6].source.clone(),
        )
    });
    assert_eq!(names, [Some("orders_2".into()), Some("totals_2".into())]);
    assert_eq!(sql, "SELECT sum(amount) AS revenue FROM {{orders_2}}");
    cx.simulate_keystrokes("z");
    assert_eq!(ids(&view, cx), all, "paste is one undo step");
}

#[gpui::test]
fn pasting_plain_sql_splits_into_cells_and_markdown_into_narrative(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    command_mode(&view, cx);
    cx.update(|_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(
            "SELECT 10;\nSELECT 20;\n".to_string(),
        ))
    });
    cx.simulate_keystrokes("shift-v");
    let src = sources(&view, cx);
    assert_eq!(src.len(), 7);
    assert_eq!(
        &src[..2],
        &["SELECT 10;".to_string(), "SELECT 20;".to_string()]
    );
    cx.update(|_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(
            "# Notes\n\n```sql\nSELECT 30\n```".to_string(),
        ))
    });
    draw(&view, cx);
    cx.simulate_keystrokes("v");
    let kinds: Vec<String> = view.read_with(cx, |nb, _| {
        nb.doc().cells().map(|c| c.kind.to_string()).collect()
    });
    assert_eq!(&kinds[1..3], &["markdown".to_string(), "sql".to_string()]);
}

#[gpui::test]
fn cut_and_duplicate(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[4], cx));
    cx.simulate_keystrokes("secondary-d");
    let after = ids(&view, cx);
    assert_eq!(after.len(), 6);
    assert_eq!(sources(&view, cx)[5], "SELECT 1");
    draw(&view, cx);
    cx.simulate_keystrokes("x");
    assert_eq!(ids(&view, cx).len(), 5);
    assert!(cx.update(|_, cx| cx.read_from_clipboard()).is_some());
}

#[gpui::test]
fn keyboard_and_drag_moves(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[4], cx));
    cx.simulate_keystrokes("alt-up alt-up");
    assert_eq!(ids(&view, cx)[2], all[4]);
    draw(&view, cx);
    cx.simulate_keystrokes("z z");
    assert_eq!(ids(&view, cx), all);
    view.update(cx, |nb, cx| {
        nb.drop_cells(vec![all[0].clone(), all[1].clone()], 5, cx)
    });
    assert_eq!(
        ids(&view, cx),
        vec![
            all[2].clone(),
            all[3].clone(),
            all[4].clone(),
            all[0].clone(),
            all[1].clone()
        ],
        "a dragged block lands where it was dropped, in order"
    );
}

#[gpui::test]
fn structural_undo_keeps_text_typed_since(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    serve_entities(&view, cx);
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell(&all[4], cx));
    cx.run_until_parked();
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&all[4]))
        .expect("editor attached")
        .0;
    view.update(cx, |nb, cx| nb.insert_cell("sql", 0, cx));
    editor.update(cx, |e, cx| {
        e.place_cursor(8, cx);
        e.insert(" + 1", cx);
    });
    cx.run_until_parked();
    assert_eq!(sources(&view, cx)[5], "SELECT 1 + 1");
    assert!(view.read_with(cx, |nb, _| nb.can_undo_structure()));
    view.update(cx, |nb, cx| nb.undo_structure(cx));
    assert_eq!(ids(&view, cx), all, "the insert is undone");
    assert_eq!(sources(&view, cx)[4], "SELECT 1 + 1", "typing stays");
    assert_eq!(
        editor.read_with(cx, |e, _| e.text().to_string()),
        "SELECT 1 + 1"
    );
    assert!(view.read_with(cx, |nb, _| nb.can_redo_structure()));
}

#[gpui::test]
fn rename_and_title_edits_undo(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.rename_cell(&all[1], "orders_v2", cx));
    assert_eq!(
        sources(&view, cx)[2],
        "SELECT sum(amount) AS revenue FROM {{orders_v2}}"
    );
    view.update(cx, |nb, cx| nb.undo_structure(cx));
    assert_eq!(
        sources(&view, cx)[2],
        "SELECT sum(amount) AS revenue FROM {{orders}}"
    );
    let name = view.read_with(cx, |nb, _| nb.model().cells[1].name.clone());
    assert_eq!(name.as_deref(), Some("orders"));
}

#[gpui::test]
fn split_at_cursor_and_merge(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut nb = doc();
    nb.cells[4].source = "SELECT 1;\nSELECT 2".into();
    let (view, cx) = open(cx, nb);
    serve_entities(&view, cx);
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell(&all[4], cx));
    cx.run_until_parked();
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&all[4]))
        .unwrap()
        .0;
    editor.update(cx, |e, cx| e.place_cursor(9, cx));
    view.update(cx, |nb, cx| nb.editing_command(EditingCommand::Split, cx));
    let src = sources(&view, cx);
    assert_eq!(
        &src[4..],
        &["SELECT 1;".to_string(), "SELECT 2".to_string()]
    );
    assert_eq!(
        ids(&view, cx)[4],
        all[4],
        "the first half keeps the cell's id"
    );
    view.update(cx, |nb, cx| {
        nb.activate_cell(&all[4], cx);
        nb.editing_command(EditingCommand::MergeBelow, cx);
    });
    assert_eq!(sources(&view, cx)[4], "SELECT 1;\n\nSELECT 2");
    assert_eq!(ids(&view, cx), all);
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[1], cx));
    cx.simulate_keystrokes("shift-m");
    assert_eq!(ids(&view, cx).len(), 4, "Shift+M merges two SQL cells");
}

#[gpui::test]
fn insert_menu_from_the_keyboard(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[0], cx));
    cx.simulate_keystrokes("i");
    assert!(view.read_with(cx, |nb, _| nb.editing.insert_menu.is_some()));
    draw(&view, cx);
    cx.simulate_keystrokes("j");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(all[0].clone()),
        "j moves the menu, not the cell"
    );
    let second = crate::notebook::cells::insertable_kinds()
        .nth(1)
        .unwrap()
        .kind();
    cx.simulate_keystrokes("enter");
    let kind = view.read_with(cx, |nb, _| nb.doc().cell(1).unwrap().kind.to_string());
    assert_eq!(kind, second);
    assert!(view.read_with(cx, |nb, _| nb.editing.insert_menu.is_none()));
}

#[gpui::test]
fn find_and_replace_across_the_notebook(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut nb = doc();
    cell(&mut nb, CellKind::Chart, None, "");
    nb.cells[5].spec = serde_json::json!({"input": "cell_3", "chart": {"title": "Revenue trend"}});
    cell(
        &mut nb,
        CellKind::Unknown("python".into()),
        None,
        "revenue = 1",
    );
    let (view, cx) = open(cx, nb);
    command_mode(&view, cx);
    cx.simulate_keystrokes("secondary-shift-h");
    draw(&view, cx);
    view.update(cx, |nb, cx| {
        nb.set_find_query("revenue", cx);
        nb.editing.find.as_mut().unwrap().replacement = "income".into();
    });
    let total = view.read_with(cx, |nb, _| nb.editing.find.as_ref().unwrap().matches.len());
    assert_eq!(
        total, 6,
        "title, narrative, SQL, chart title and the unknown cell"
    );
    view.update(cx, |nb, cx| nb.find_step(1, cx));
    view.update(cx, |nb, cx| nb.replace_all(cx));
    let (title, chart, python) = view.read_with(cx, |nb, _| {
        let m = nb.model();
        (
            m.title.clone(),
            m.cells[5].spec["chart"]["title"].clone(),
            m.cells[6].source.clone(),
        )
    });
    assert_eq!(title, "income review");
    assert_eq!(chart, "income trend");
    assert_eq!(
        python, "revenue = 1",
        "cells this build cannot edit are left alone"
    );
    view.update(cx, |nb, cx| nb.undo_structure(cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.model().title.clone()),
        "Revenue review",
        "replace all is one undo step"
    );
}

#[gpui::test]
fn find_searches_result_values_only_when_asked(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    let output = sqlly_services::notebook::CellOutput::capped(
        vec![sqlly_core::ResultColumn {
            name: "region".into(),
            data_type: "text".into(),
            nullable: true,
        }],
        vec![vec![sqlly_core::CellValue::String("Northwind".into())]],
        None,
        10,
        0,
        "run_1",
        sqlly_services::notebook::Fingerprint("f".into()),
        Default::default(),
    );
    view.update(cx, |nb, cx| {
        nb.outputs.publish(&all[1], output);
        nb.open_find(false, cx);
        nb.set_find_query("northwind", cx);
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.editing.find.as_ref().unwrap().matches.len()),
        0
    );
    view.update(cx, |nb, cx| {
        nb.set_find_options(|f| f.include_data = true, cx)
    });
    let matches = view.read_with(cx, |nb, _| {
        nb.editing.find.as_ref().unwrap().matches.clone()
    });
    assert_eq!(matches.len(), 1);
    assert!(!matches[0].replaceable());
}

#[gpui::test]
fn bookmarks_jump_and_list(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| {
        nb.toggle_bookmark(vec![all[1].clone()], cx);
        nb.toggle_bookmark(vec![all[4].clone()], cx);
    });
    assert!(view.read_with(cx, |nb, _| nb.cell_flag(&all[1], BOOKMARK_KEY)));
    assert!(
        !view.read_with(cx, |nb, _| nb.can_undo_structure()),
        "bookmarks are not undo steps"
    );
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[0], cx));
    cx.simulate_keystrokes("f2");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(all[1].clone())
    );
    draw(&view, cx);
    cx.simulate_keystrokes("f2");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(all[4].clone())
    );
    draw(&view, cx);
    cx.simulate_keystrokes("f2");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(all[1].clone()),
        "wraps"
    );
    view.update(cx, |nb, cx| {
        nb.editing_command(EditingCommand::ShowBookmarks, cx)
    });
    draw(&view, cx);
    let saved = view.read_with(cx, |nb, _| nb.model().to_canonical_json());
    assert!(
        saved.contains("\"bookmarked\": true"),
        "bookmarks are saved with the notebook"
    );
}

#[gpui::test]
fn folding_a_heading_hides_its_section_and_navigation_skips_it(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.toggle_fold(Some(all[0].clone()), cx));
    let hidden = view.read_with(cx, |nb, _| {
        (0..5).map(|i| nb.is_folded_away(i)).collect::<Vec<_>>()
    });
    assert_eq!(
        hidden,
        vec![false, true, true, true, true],
        "# folds through the ## below it"
    );
    view.update(cx, |nb, cx| {
        nb.toggle_fold(Some(all[0].clone()), cx);
        nb.toggle_fold(Some(all[3].clone()), cx);
    });
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[2], cx));
    cx.simulate_keystrokes("j j");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(all[3].clone()),
        "j skips folded cells"
    );
    assert!(view.read_with(cx, |nb, _| nb.cell_flag(&all[3], FOLDED_KEY)));
    view.update(cx, |nb, cx| nb.go_to_cell(&all[4], cx));
    assert!(
        !view.read_with(cx, |nb, _| nb.is_folded_away(4)),
        "going to a folded cell unfolds it"
    );
}

#[gpui::test]
fn pinned_outputs_strip_renders(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let all = ids(&view, cx);
    command_mode(&view, cx);
    view.update(cx, |nb, cx| nb.activate_cell(&all[1], cx));
    cx.simulate_keystrokes("p");
    assert!(view.read_with(cx, |nb, _| nb.cell_flag(&all[1], PINNED_KEY)));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.pinned()),
        vec![all[1].clone()]
    );
    draw(&view, cx);
}

#[gpui::test]
fn editing_works_in_zen_and_dense_modes(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    view.update(cx, |nb, cx| {
        nb.toggle_zen(cx);
        nb.toggle_dense(cx);
    });
    command_mode(&view, cx);
    cx.simulate_keystrokes("secondary-a");
    draw(&view, cx);
    cx.simulate_keystrokes("secondary-f");
    draw(&view, cx);
    assert!(view.read_with(cx, |nb, _| nb.editing.find.is_some()));
    assert_eq!(view.read_with(cx, |nb, _| nb.selection().len()), 5);
}

#[gpui::test]
fn cells_have_spoken_labels(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    let label = view.read_with(cx, |nb, _| nb.cell_accessibility_label(1));
    assert!(label.contains("orders"), "{label}");
    assert!(label.contains("2 of 5"), "{label}");
}

#[gpui::test]
fn editors_complete_references_and_hover_shows_the_cell(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    serve_entities(&view, cx);
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell(&all[4], cx));
    cx.run_until_parked();
    draw(&view, cx);
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&all[4]))
        .unwrap()
        .0;
    editor.update(cx, |e, cx| {
        e.set_text("SELECT * FROM {{to", cx);
        e.place_cursor(18, cx);
        e.request_completions_for_test(cx);
    });
    let labels = editor.read_with(cx, |e, _| e.completion_labels_for_test());
    assert_eq!(labels, vec!["totals".to_string()]);
    editor.update(cx, |e, cx| e.accept_completion_for_test(cx));
    assert_eq!(
        editor.read_with(cx, |e, _| e.text().to_string()),
        "SELECT * FROM {{totals}}"
    );
    let card = editor.read_with(cx, |e, _| e.notebook_hover_for_test(16));
    let Some(sqlly_intel::intellisense::HoverInfo::Relation { name, columns, .. }) = card else {
        panic!("a reference hover card, got {card:?}");
    };
    assert_eq!(name, "totals");
    assert_eq!(
        columns.len(),
        1,
        "columns come from the query before it runs"
    );
    assert_eq!(columns[0].name, "revenue");
}

#[gpui::test]
fn a_reference_hover_card_overflows_a_short_cell_editor(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    serve_entities(&view, cx);
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell(&all[4], cx));
    cx.run_until_parked();
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&all[4]))
        .unwrap()
        .0;
    editor.update(cx, |e, cx| {
        e.set_text("SELECT *\nFROM {{totals}}\nWHERE 1 = 1\nORDER BY 1", cx);
        e.show_notebook_hover_for_test(16);
    });
    // A real frame of the window: the card is a deferred layer, which the
    // window paints after its tree.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let (shown, all_lines) = editor
        .read_with(cx, |e, _| e.hover_card_lines_for_test())
        .expect("the card painted");
    assert!(
        all_lines >= 3,
        "kind and name, state, then the columns: {all_lines}"
    );
    assert_eq!(shown, all_lines, "every line of the card shows");
}

#[gpui::test]
fn markdown_slash_menu_inserts_a_cell_below(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    serve_entities(&view, cx);
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell(&all[3], cx));
    cx.run_until_parked();
    draw(&view, cx);
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&all[3]))
        .unwrap()
        .0;
    let label = crate::localization::format(
        "notebook.editing.slash.new_cell",
        &[(
            "kind",
            crate::localization::text("notebook.shell.cell.kind_sql"),
        )],
    );
    let typed = format!("## Detail\n/{}", &label[..3]);
    editor.update(cx, |e, cx| {
        e.set_text(typed.clone(), cx);
        e.place_cursor(typed.len(), cx);
        e.request_completions_for_test(cx);
    });
    let labels = editor.read_with(cx, |e, _| e.completion_labels_for_test());
    let position = labels
        .iter()
        .position(|l| *l == label)
        .expect("new SQL cell item");
    for _ in 0..position {
        editor.update(cx, |e, cx| e.move_completion_down_for_test(cx));
    }
    editor.update(cx, |e, cx| e.accept_completion_for_test(cx));
    cx.run_until_parked();
    assert_eq!(ids(&view, cx).len(), 6, "a cell was inserted below");
    assert_eq!(
        editor.read_with(cx, |e, _| e.text().trim_end().to_string()),
        "## Detail"
    );
}

#[gpui::test]
fn typing_a_slash_on_an_empty_markdown_line_draws_the_menu(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, doc());
    serve_entities(&view, cx);
    let all = ids(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell(&all[3], cx));
    cx.run_until_parked();
    draw(&view, cx);
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&all[3]))
        .unwrap()
        .0;
    let typed = "## Detail\n/".to_string();
    editor.update(cx, |e, cx| {
        e.set_text(typed.clone(), cx);
        e.place_cursor(typed.len(), cx);
        e.request_completions_for_test(cx);
    });
    let labels = editor.read_with(cx, |e, _| e.completion_labels_for_test());
    assert!(labels.contains(&crate::localization::text("notebook.editing.slash.table")));
    assert!(editor.read_with(cx, |e, _| e.completion_popup_visible()));
    // Painting the open menu used to panic on the table snippet's newlines.
    draw(&view, cx);
    for item in super::ui::slash_items() {
        assert!(
            !item.detail.is_empty() && !item.detail.contains('\n'),
            "{} needs a one-line detail",
            item.label
        );
    }
}

#[gpui::test]
fn the_screenshot_fixture_opens_with_its_bookmarks_pins_and_fold(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks/editing-review.sqllynb");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("{P_DEV}", "dev-profile");
    let loaded = sqlly_services::notebook::load(
        Some("editing-review.sqllynb"),
        &text,
        &mut sqlly_services::notebook::RandomIds,
        None,
    )
    .unwrap();
    let (view, cx) = open(cx, loaded.doc);
    draw(&view, cx);
    let (marks, pins, hidden) = view.read_with(cx, |nb, _| {
        (
            nb.bookmarked().len(),
            nb.pinned().len(),
            (0..nb.doc().len())
                .filter(|i| nb.is_folded_away(*i))
                .count(),
        )
    });
    assert_eq!((marks, pins, hidden), (2, 1, 2));
}

fn markdown_doc() -> NotebookDoc {
    use sqlly_services::notebook::params::{ParamType, ParameterDef};
    let mut nb = NotebookDoc::empty("nb_markdown_help", "Markdown help");
    cell(&mut nb, CellKind::Markdown, None, "# Summary");
    cell(&mut nb, CellKind::Scalar, Some("avg_order"), "1");
    cell(&mut nb, CellKind::Sql, Some("orders"), "SELECT 1 AS n");
    let mut picture = Cell::new("trend", CellKind::Attachment, ExecutionPolicy::default());
    picture.spec =
        serde_json::json!({"format": "png", "file_name": "trend.png", "path": "trend.png"});
    nb.cells.push(picture);
    let mut data = Cell::new("sales", CellKind::Attachment, ExecutionPolicy::default());
    data.spec = serde_json::json!({"format": "csv", "file_name": "sales.csv", "path": "sales.csv"});
    nb.cells.push(data);
    let mut region = ParameterDef::new("region", ParamType::String);
    region.default = Some(serde_json::json!("West"));
    let mut token = ParameterDef::new("api_key", ParamType::String);
    token.default = Some(serde_json::json!("s3cret-value"));
    token.sensitive = true;
    nb.parameters = vec![region, token];
    nb
}

/// The Markdown cell's live editor, with the formula `avg_order` holding 42.
fn markdown_editor(
    cx: &mut TestAppContext,
) -> (
    Entity<NotebookView>,
    Entity<SqlEditor>,
    &mut VisualTestContext,
) {
    let (view, cx) = open(cx, markdown_doc());
    serve_entities(&view, cx);
    view.update(cx, |nb, _| {
        nb.outputs.publish(
            "cell_2",
            sqlly_services::notebook::CellOutput {
                columns: vec![sqlly_core::ResultColumn {
                    name: "value".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                }],
                rows: std::sync::Arc::new(vec![vec![sqlly_core::CellValue::I64(42)]]),
                completeness: sqlly_services::notebook::Completeness::Complete,
                row_count: 1,
                elapsed_ms: 1,
                run_id: "r_1".into(),
                fingerprint: sqlly_services::notebook::fingerprint::Fingerprint("f".into()),
                provenance: Default::default(),
                freshness: Default::default(),
            },
        );
    });
    view.update(cx, |nb, cx| nb.edit_cell("cell_1", cx));
    cx.run_until_parked();
    draw(&view, cx);
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities("cell_1"))
        .unwrap()
        .0;
    (view, editor, cx)
}

fn completions_at_end(
    editor: &Entity<SqlEditor>,
    text: &str,
    cx: &mut VisualTestContext,
) -> Vec<(String, String)> {
    editor.update(cx, |e, cx| {
        e.set_text(text, cx);
        e.place_cursor(text.len(), cx);
        e.request_completions_for_test(cx);
    });
    editor.read_with(cx, |e, _| {
        e.completion_items_for_test()
            .into_iter()
            .map(|i| (i.label, i.detail))
            .collect()
    })
}

#[gpui::test]
fn markdown_braces_list_values_and_parameters_with_sensitive_ones_hidden(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, editor, cx) = markdown_editor(cx);
    assert_eq!(
        editor.read_with(cx, |e, _| e.language()),
        crate::editor::EditorLanguage::Markdown
    );
    let items = completions_at_end(&editor, "Average order: {{", cx);
    let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        vec!["avg_order", "param."],
        "formulas and parameters only"
    );
    assert!(items[0].1.contains("42"), "the value: {}", items[0].1);

    let items = completions_at_end(&editor, "Region {{param.", cx);
    let detail = |name: &str| {
        items
            .iter()
            .find(|(l, _)| l == name)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("{name} offered: {items:?}"))
    };
    assert!(detail("param.region").contains("West"));
    let hidden = detail("param.api_key");
    assert!(
        !hidden.contains("s3cret"),
        "a sensitive value never shows: {hidden}"
    );

    let first = items[0].0.clone();
    editor.update(cx, |e, cx| e.accept_completion_for_test(cx));
    let text = editor.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(text, format!("Region {{{{{first}}}}}"));
    let card = editor.update(cx, |e, cx| {
        e.set_text("Key {{param.api_key}} and {{avg_order}}", cx);
        (e.hover_at_for_test(8), e.hover_at_for_test(30))
    });
    let Some(sqlly_intel::intellisense::HoverInfo::Relation { details, .. }) = card.0 else {
        panic!("a parameter card, got {:?}", card.0);
    };
    assert!(details.iter().all(|d| !d.contains("s3cret")), "{details:?}");
    let Some(sqlly_intel::intellisense::HoverInfo::Relation { name, details, .. }) = card.1 else {
        panic!("a formula card, got {:?}", card.1);
    };
    assert_eq!(name, "avg_order");
    assert!(details.iter().any(|d| d.contains("42")), "{details:?}");

    // A value computed from truncated rows says so.
    view.update(cx, |nb, _| {
        let mut partial = (*nb.outputs.get("cell_2").unwrap()).clone();
        partial.completeness = sqlly_services::notebook::Completeness::Truncated { cap: 2 };
        nb.outputs.publish("cell_2", partial);
    });
    draw(&view, cx);
    let items = completions_at_end(&editor, "{{avg", cx);
    let note = crate::localization::text("notebook.markdown_editor.intel.partial");
    assert!(items[0].1.contains(&note), "{:?}", items[0]);
}

#[gpui::test]
fn markdown_pictures_complete_picture_attachments(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (_view, editor, cx) = markdown_editor(cx);
    let items = completions_at_end(&editor, "![Trend](", cx);
    let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["trend"], "picture attachments only");
    editor.update(cx, |e, cx| e.accept_completion_for_test(cx));
    assert_eq!(
        editor.read_with(cx, |e, _| e.text().to_string()),
        "![Trend](attachment:trend"
    );
}

#[gpui::test]
fn markdown_flags_unknown_references_gently(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (_view, editor, cx) = markdown_editor(cx);
    editor.update(cx, |e, cx| {
        e.set_text("", cx);
        e.insert(
            "{{avg_order}} {{nope}} {{param.zone}} ![x](attachment:gone)",
            cx,
        );
    });
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    let found: Vec<(String, crate::editor::DiagnosticSeverity)> = editor.read_with(cx, |e, _| {
        e.validation_diagnostics()
            .iter()
            .map(|d| (e.text()[d.start_byte..d.end_byte].to_string(), d.severity))
            .collect()
    });
    use crate::editor::DiagnosticSeverity::Warning;
    assert_eq!(
        found,
        vec![
            ("{{nope}}".to_string(), Warning),
            ("{{param.zone}}".to_string(), Warning),
            ("attachment:gone".to_string(), Warning),
        ]
    );
    let message = editor.read_with(cx, |e, _| e.validation_diagnostics()[0].message.clone());
    assert_eq!(
        message,
        crate::localization::format(
            "notebook.markdown_editor.diag.unknown_reference",
            &[("name", "nope".to_string())]
        )
    );
}

#[gpui::test]
fn markdown_lists_continue_nest_and_brackets_pair(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (_view, editor, cx) = markdown_editor(cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    let text = |cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.text().to_string());
    editor.update(cx, |e, cx| {
        e.set_text("- milk", cx);
        e.place_cursor(6, cx);
    });
    cx.dispatch_action(crate::editor::Enter);
    assert_eq!(text(cx), "- milk\n- ");
    cx.simulate_input("eggs");
    cx.dispatch_action(crate::editor::Tab);
    let nested = text(cx);
    assert!(
        nested.starts_with("- milk\n") && nested.ends_with("- eggs"),
        "{nested:?}"
    );
    assert_ne!(nested, "- milk\n- eggs", "Tab nests the item");
    cx.dispatch_action(crate::editor::ShiftTab);
    assert_eq!(text(cx), "- milk\n- eggs");
    cx.dispatch_action(crate::editor::Enter);
    cx.dispatch_action(crate::editor::Enter);
    assert_eq!(text(cx), "- milk\n- eggs\n", "an empty item ends the list");

    editor.update(cx, |e, cx| {
        e.set_text("1. one", cx);
        e.place_cursor(6, cx);
    });
    cx.dispatch_action(crate::editor::Enter);
    assert_eq!(text(cx), "1. one\n2. ");

    editor.update(cx, |e, cx| {
        e.set_text("see ", cx);
        e.place_cursor(4, cx);
    });
    cx.simulate_input("[docs");
    cx.simulate_input("]");
    cx.simulate_input("(");
    assert_eq!(text(cx), "see [docs]()", "brackets pair and step over");
    cx.dispatch_action(crate::editor::Backspace);
    assert_eq!(text(cx), "see [docs]", "Backspace removes an empty pair");
}

#[gpui::test]
fn the_markdown_editor_screenshot_fixture_completes_its_formula_value(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks/markdown-editor-values.sqllynb");
    let text = std::fs::read_to_string(&path).unwrap();
    let loaded = sqlly_services::notebook::load(
        Some("markdown-editor-values.sqllynb"),
        &text,
        &mut sqlly_services::notebook::RandomIds,
        None,
    )
    .unwrap();
    let (view, cx) = open(cx, loaded.doc);
    serve_entities(&view, cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    cx.run_until_parked();
    view.update(cx, |nb, cx| nb.edit_cell("cell_2", cx));
    cx.run_until_parked();
    draw(&view, cx);
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities("cell_2"))
        .unwrap()
        .0;
    let items = completions_at_end(&editor, "Average {{", cx);
    let avg = items
        .iter()
        .find(|(l, _)| l == "avg_order")
        .unwrap_or_else(|| panic!("the formula is offered: {items:?}"));
    assert!(avg.1.contains("117.11"), "with its value: {}", avg.1);
    let items = completions_at_end(&editor, "Token {{param.", cx);
    let token = items
        .iter()
        .find(|(l, _)| l == "param.api_token")
        .unwrap_or_else(|| panic!("the sensitive parameter is offered: {items:?}"));
    let hidden = crate::localization::format(
        "notebook.markdown_editor.intel.param_hidden",
        &[("type", "")],
    );
    assert!(token.1.ends_with(hidden.trim()), "{}", token.1);
}
