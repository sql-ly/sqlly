//! Notebook cell editors as the real host makes them (`make_cell_editor`,
//! with IntelliSense services): a Markdown cell edits as Markdown with no
//! SQL IntelliSense, while a SQL cell in the same notebook keeps SQL
//! completion, hover and `{{…}}` references.

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use sqlly_intel::intellisense::{CompletionItem, CompletionKind};
use sqlly_services::notebook::{Cell, CellKind, ExecutionPolicy, NotebookDoc};

use super::AppView;
use crate::editor::{EditorLanguage, SqlEditor};
use crate::notebook_view::NotebookView;

fn open_app(cx: &mut TestAppContext) -> (Entity<AppView>, &mut VisualTestContext) {
    crate::ensure_test_tokio_runtime();
    // `AppView::new` starts launch-time work that completes on a tokio
    // worker (see `close_three_tabs_tests`).
    cx.dispatcher.allow_parking();
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        let status = crate::update_status::handle(cx);
        status.update(cx, |s, _| {
            s.state = crate::update_status::UpdateCheckState::Checking;
        });
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let services = sqlly_services::Services::new();
        let app = cx.new(|cx| AppView::new(services, window, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<AppView>()
            .expect("root hosts the AppView")
    });
    (app, vcx)
}

fn doc() -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nb_md_host", "Markdown help");
    let mut cell = |id: &str, kind, name: Option<&str>, source: &str| {
        let mut c = Cell::new(id, kind, ExecutionPolicy::default());
        c.name = name.map(str::to_string);
        c.source = source.to_string();
        nb.cells.push(c);
    };
    cell("notes", CellKind::Markdown, None, "# Notes");
    cell(
        "orders",
        CellKind::Sql,
        Some("orders"),
        "SELECT 1 AS order_id",
    );
    cell("query", CellKind::Sql, None, "SELECT 2");
    nb
}

/// The host-made editor of cell `id`, after asking the notebook to edit it
/// and drawing a frame (which pushes the notebook's names to its editors).
fn cell_editor(
    notebook: &Entity<NotebookView>,
    id: &str,
    cx: &mut VisualTestContext,
) -> Entity<SqlEditor> {
    notebook.update(cx, |nb, cx| nb.edit_cell(id, cx));
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    notebook
        .read_with(cx, |nb, _| nb.cell_entities(id))
        .expect("the host made the cell's editor")
        .0
}

fn complete(
    editor: &Entity<SqlEditor>,
    text: &str,
    cx: &mut VisualTestContext,
) -> Vec<CompletionItem> {
    editor.update(cx, |e, cx| {
        e.set_text(text, cx);
        e.place_cursor(text.len(), cx);
        e.request_completions_for_test(cx);
    });
    cx.run_until_parked();
    editor.read_with(cx, |e, _| e.completion_items_for_test())
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

#[gpui::test]
fn markdown_cells_get_no_sql_intellisense_and_sql_cells_keep_it(cx: &mut TestAppContext) {
    let (app, cx) = open_app(cx);
    let notebook = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let tab = this.new_notebook_tab(doc(), None, cx);
            this.tabs[tab].notebook.clone().expect("a notebook tab")
        })
    });

    let markdown = cell_editor(&notebook, "notes", cx);
    assert_eq!(
        markdown.read_with(cx, |e, _| e.language()),
        EditorLanguage::Markdown
    );
    for text in [
        "SEL",
        "SELECT * FROM o",
        "SELECT * FROM {{orders}} o\nWHERE o.",
    ] {
        let items = complete(&markdown, text, cx);
        assert!(
            items.iter().all(|i| matches!(
                i.kind,
                CompletionKind::Snippet | CompletionKind::Value | CompletionKind::Parameter
            )) && !labels(&items)
                .iter()
                .any(|l| l.eq_ignore_ascii_case("SELECT")),
            "only Markdown help in a Markdown cell for {text:?}: {:?}",
            labels(&items)
        );
    }
    // Typing (the automatic pass) stays quiet outside Markdown triggers.
    markdown.update(cx, |e, cx| {
        e.set_text("", cx);
        e.insert("SEL", cx);
    });
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    assert!(!markdown.read_with(cx, |e, _| e.completion_popup_visible()));
    let hover = markdown.update(cx, |e, cx| {
        e.set_text("SELECT count(*) FROM orders", cx);
        e.hover_at_for_test(2)
    });
    assert_eq!(hover, None, "no SQL hover card in Markdown");
    let items = complete(&markdown, "Orders: {{", cx);
    assert!(
        !labels(&items).contains(&"orders"),
        "a SQL cell is not a value Markdown can show: {:?}",
        labels(&items)
    );

    let sql = cell_editor(&notebook, "query", cx);
    assert_eq!(sql.read_with(cx, |e, _| e.language()), EditorLanguage::Sql);
    let items = complete(&sql, "SEL", cx);
    assert!(
        labels(&items)
            .iter()
            .any(|l| l.eq_ignore_ascii_case("SELECT")),
        "SQL cells still complete keywords: {:?}",
        labels(&items)
    );
    let items = complete(&sql, "SELECT * FROM {{or", cx);
    assert_eq!(labels(&items), vec!["orders"], "and cell references");
    let hover = sql.update(cx, |e, cx| {
        e.set_text("SELECT count(*) FROM t", cx);
        e.hover_at_for_test(8)
    });
    assert!(hover.is_some(), "and SQL hover cards");
}
