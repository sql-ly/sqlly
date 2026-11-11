use super::element::{EDITOR_FALLBACK_VISIBLE_LINES, EDITOR_OVERSCAN_LINES, visible_line_range};
use super::*;
use crate::preferences::{EditorPreferences, IndentStyle};
use sqlly_intel::intellisense::{CompletionObject, CompletionSource, HoverColumn, HoverInfo};
use sqlly_services::{ProcedureInfo, ProcedureParameter};
use std::collections::BTreeSet;
use std::sync::Arc;

fn table_fragment(table: &str, columns: &[&str]) -> sqlly_lang::sqlmodel::ObjectFragment {
    use sqlly_lang::sqlmodel::{
        ColumnDef, Dialect, Fidelity, ObjectBody, ObjectFragment, ObjectKind, ObjectRef,
        Provenance, SqlName, TableBody,
    };

    let dialect = Dialect::SqlServer;
    ObjectFragment {
        reference: ObjectRef::new(
            None,
            Some(SqlName::new("dbo", dialect, false)),
            SqlName::new(table, dialect, false),
        ),
        kind: ObjectKind::Table,
        dialect,
        body: ObjectBody::Table(TableBody {
            columns: columns
                .iter()
                .enumerate()
                .map(|(ordinal, column)| ColumnDef {
                    name: SqlName::new(*column, dialect, false),
                    ty: None,
                    nullable: None,
                    default: None,
                    identity: false,
                    generated: None,
                    collation: None,
                    ordinal,
                })
                .collect(),
            ..TableBody::default()
        }),
        provenance: Provenance::Database {
            profile: "test".into(),
        },
        fidelity: Fidelity::Exact,
        references: Vec::new(),
    }
}

fn snapshot_from_fragments(
    fragments: Vec<sqlly_lang::sqlmodel::ObjectFragment>,
) -> sqlly_lang::sqlmodel::ModelSnapshot {
    sqlly_lang::sqlmodel::ModelSnapshot::empty()
        .apply(
            1,
            fragments,
            &BTreeSet::new(),
            sqlly_lang::sqlmodel::IncompleteRegions::default(),
        )
        .0
}

fn schema_snapshot(tables: &[(&str, &[&str])]) -> sqlly_lang::sqlmodel::ModelSnapshot {
    snapshot_from_fragments(
        tables
            .iter()
            .map(|(table, columns)| table_fragment(table, columns))
            .collect(),
    )
}

/// Regression (docs screenshot `intellisense-fk-name`): value lookup on an
/// FK id column listed the raw ids because the editor never consulted the
/// model's foreign keys. It now follows the FK to the referenced table's name
/// column: the popup lists and matches names, and accepting inserts the id.
#[gpui::test]
fn fk_value_lookup_searches_names_and_inserts_ids(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "SELECT * FROM Products WHERE CategoryID = ";
    let ddl = "CREATE TABLE dbo.Categories (CategoryID int PRIMARY KEY, CategoryName nvarchar(15), Description ntext);\nCREATE TABLE dbo.Products (ProductID int PRIMARY KEY, ProductName nvarchar(40), CategoryID int REFERENCES dbo.Categories(CategoryID))";
    let dialect = sqlly_lang::sqlmodel::Dialect::SqlServer;
    let fragments = sqlly_lang::sqlparse::lower(
        &sqlly_lang::sqlparse::parse(ddl, dialect),
        sqlly_lang::sqlmodel::Provenance::Database {
            profile: "test".into(),
        },
    );
    let snapshot = snapshot_from_fragments(fragments);

    view.update(cx, |editor, cx| {
        editor.schema_snapshot = Arc::new(snapshot);
        editor.set_text(sql, cx);
        editor.cursor = sql.len();
        let context = ValueContext {
            column: Some("CategoryID".into()),
            table: Some("Products".into()),
            schema: None,
            operator: sqlly_services::ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: sql.len(),
            replace_end: sql.len(),
            has_opening_quote: false,
        };
        let pivot = editor
            .fk_pivot_for_value_context(&context)
            .expect("CategoryID follows its foreign key");
        assert_eq!(pivot.key_column, "CategoryID");
        assert_eq!(pivot.display_column, "CategoryName");
        // A LIKE slot keeps the raw ids: a pattern over ids is no name search.
        let like = ValueContext {
            operator: sqlly_services::ValueOperator::Like,
            ..context.clone()
        };
        assert!(editor.fk_pivot_for_value_context(&like).is_none());

        editor.active_value_lookup = Some(context);
        editor.value_lookup_fk = Some(pivot);
        editor.value_lookup_token = 1;
        editor.value_lookup_pending = true;
        editor.apply_value_lookup_entries(
            1,
            vec![
                sqlly_services::ValueLookupEntry {
                    value: "1".into(),
                    label: Some("Beverages".into()),
                },
                sqlly_services::ValueLookupEntry {
                    value: "2".into(),
                    label: Some("Condiments".into()),
                },
            ],
            cx,
        );
    });

    // Typing part of a name narrows by the label.
    view.update(cx, |editor, cx| editor.insert("Bev", cx));
    view.read_with(cx, |editor, _| {
        let labels: Vec<&str> = editor
            .completions
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, vec!["Beverages"]);
        let item = &editor.completions[0];
        assert_eq!(item.insert_text, "1 /* Beverages */");
        assert!(
            item.detail.contains("Categories.CategoryName"),
            "{}",
            item.detail
        );
        assert_eq!(item.replace_span, Some((sql.len(), sql.len() + 3)));
    });
}

/// Feedback ifb_2fc6: the trigger fetch is capped at 100 rows, so a column
/// with more distinct values than that is only sampled. Typing `Ton` after
/// `ii.Name like` matched nothing in the sample and the popup fell back to
/// the bare `'%%'` template. A capped sample that the typed text has
/// outgrown must re-query with the text as a server-side filter; a complete
/// set, or a narrower filter over a complete filtered set, stays local.
#[gpui::test]
fn value_lookup_requeries_when_typed_text_outgrows_a_capped_sample(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let requests: std::rc::Rc<std::cell::RefCell<Vec<(u64, String)>>> = Default::default();
    let seen = requests.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &SqlEditorEvent, _cx| {
            if let SqlEditorEvent::ValueCompletionsRequested {
                request_token,
                partial,
                ..
            } = event
            {
                seen.borrow_mut().push((*request_token, partial.clone()));
            }
        })
        .detach();
    });

    let sql = "SELECT * FROM Inventory.Item ii WHERE ii.Name like ";
    let capped_sample: Vec<String> = (0..100).map(|i| format!("Apple {i:03}")).collect();
    view.update(cx, |editor, cx| {
        editor.set_text(sql, cx);
        editor.cursor = sql.len();
        editor.active_value_lookup = Some(ValueContext {
            column: Some("Name".into()),
            table: Some("Item".into()),
            schema: Some("Inventory".into()),
            operator: sqlly_services::ValueOperator::Like,
            partial_value: String::new(),
            replace_start: sql.len(),
            replace_end: sql.len(),
            has_opening_quote: false,
        });
        editor.value_lookup_token = 1;
        editor.value_lookup_pending = true;
        let entries = capped_sample
            .iter()
            .cloned()
            .map(sqlly_services::ValueLookupEntry::plain)
            .collect();
        editor.apply_value_lookup_fetch(1, entries, String::new(), true, cx);
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.completions.len(), 100);
        assert!(
            requests.borrow().is_empty(),
            "no refetch for the trigger result"
        );
    });

    // `App` matches the sample, but a capped sample cannot prove it holds
    // every value containing `App`: the matches show at once and a filtered
    // fetch is requested behind them.
    view.update(cx, |editor, cx| editor.insert("App", cx));
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.completions.len(), 100);
        assert!(editor.value_lookup_pending);
        let reqs = requests.borrow();
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        assert_eq!(reqs[0], (2, "App".to_string()));
    });
    view.update(cx, |editor, cx| {
        let entries = capped_sample
            .iter()
            .cloned()
            .map(sqlly_services::ValueLookupEntry::plain)
            .collect();
        editor.apply_value_lookup_fetch(2, entries, "App".into(), true, cx);
    });

    // Backspace to nothing, then type `Ton`. The first backspace (`Ap`) is
    // not a narrowing of `App`, so it asks for a fresh set; the keystrokes
    // that follow while that fetch is in flight do not pile on requests.
    view.update(cx, |editor, cx| {
        for _ in 0..3 {
            editor.delete_backward(cx);
        }
        editor.insert("Ton", cx);
    });
    view.read_with(cx, |editor, _| {
        assert!(editor.value_lookup_pending);
        let reqs = requests.borrow();
        assert_eq!(reqs.len(), 2, "{reqs:?}");
        assert_eq!(reqs[1], (3, "Ap".to_string()));
    });

    // The stale `Ap` set lands: the text has moved on to `Ton`, which it
    // cannot answer, so the editor asks once more for the current text.
    view.update(cx, |editor, cx| {
        editor.apply_value_lookup_fetch(
            3,
            vec![sqlly_services::ValueLookupEntry::plain("Apricot")],
            "Ap".into(),
            false,
            cx,
        );
    });
    view.read_with(cx, |editor, _| {
        assert!(editor.value_lookup_pending);
        let reqs = requests.borrow();
        assert_eq!(reqs.len(), 3, "{reqs:?}");
        assert_eq!(reqs[2], (4, "Ton".to_string()));
    });

    // The `Ton` fetch lands complete (under the cap).
    view.update(cx, |editor, cx| {
        editor.apply_value_lookup_fetch(
            4,
            vec![sqlly_services::ValueLookupEntry::plain("Tonic Water")],
            "Ton".into(),
            false,
            cx,
        );
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(requests.borrow().len(), 3);
        assert!(!editor.value_lookup_pending);
        let labels: Vec<&str> = editor
            .completions
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, vec!["Tonic Water"]);
        assert_eq!(editor.completions[0].insert_text, "'%Tonic Water%'");
    });
    // Narrowing a complete filtered set stays local.
    view.update(cx, |editor, cx| editor.insert("i", cx));
    view.read_with(cx, |editor, _| {
        assert_eq!(requests.borrow().len(), 3);
        assert_eq!(editor.completions.len(), 1);
    });

    // Backspacing past the fetched filter: `Ton` is still the fetched set,
    // but `To` no longer contains it, so a fresh set is fetched there.
    view.update(cx, |editor, cx| {
        editor.delete_backward(cx);
        editor.delete_backward(cx);
    });
    view.read_with(cx, |_editor, _| {
        let reqs = requests.borrow();
        assert_eq!(reqs.len(), 4, "{reqs:?}");
        assert_eq!(reqs[3], (5, "To".to_string()));
    });
}

#[gpui::test]
fn value_lookup_filters_as_user_types(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "SELECT * FROM Accounting.Account WHERE Name = ";

    view.update(cx, |editor, cx| {
        editor.set_text(sql, cx);
        editor.cursor = sql.len();
        editor.active_value_lookup = Some(ValueContext {
            column: Some("Name".into()),
            table: Some("Account".into()),
            schema: Some("Accounting".into()),
            operator: sqlly_services::ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: sql.len(),
            replace_end: sql.len(),
            has_opening_quote: false,
        });
        editor.value_lookup_token = 1;
        editor.value_lookup_pending = true;
        editor.apply_value_lookup_results(
            1,
            vec!["Checking".into(), "Savings".into(), "Cash".into()],
            cx,
        );
    });

    view.read_with(cx, |editor, _| {
        assert!(editor.completion_visible);
        assert_eq!(editor.completions.len(), 3);
        assert!(!editor.value_lookup_pending);
    });

    // Typing after the popup opens narrows the list locally.
    view.update(cx, |editor, cx| editor.insert("Ch", cx));

    view.read_with(cx, |editor, _| {
        let labels: Vec<&str> = editor
            .completions
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, vec!["Checking"]);
        let item = &editor.completions[0];
        assert_eq!(item.insert_text, "'Checking'");
        // The replace span covers the typed partial so accepting the
        // completion replaces "Ch" with the full quoted literal.
        assert_eq!(item.replace_span, Some((sql.len(), sql.len() + 2)));
    });

    // Deleting back widens the list again.
    view.update(cx, |editor, cx| editor.delete_backward(cx));

    view.read_with(cx, |editor, _| {
        let labels: Vec<&str> = editor
            .completions
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, vec!["Checking", "Cash"]);
    });
}

#[gpui::test]
fn editor_preview_is_a_scalable_escaped_svg(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT '<tag>&';", cx);
    });

    view.read_with(cx, |editor, _| {
        let svg = String::from_utf8(editor.preview_svg()).unwrap();
        assert!(svg.contains(r#"viewBox="0 0 "#));
        assert!(svg.contains(r#"preserveAspectRatio="xMidYMid meet""#));
        assert!(svg.contains("&lt;tag&gt;&amp;"));
        assert!(!svg.contains("<tag>"));
    });
}

#[gpui::test]
fn pasting_emoji_keeps_utf8_text_renderable(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT 😊", cx);
        // Reproduce the invalid offset formerly returned by reverse
        // hit-testing on a tab-expanded line containing multibyte text.
        editor.cursor = editor.content.len() - 1;
    });
    cx.run_until_parked();
    cx.update(|_window, app| {
        app.write_to_clipboard(gpui::ClipboardItem::new_string("🚀".to_string()));
    });
    cx.dispatch_action(Paste);
    cx.run_until_parked();

    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT 🚀");
        assert_eq!(editor.cursor(), "SELECT 🚀".len());
    });
}

#[gpui::test]
fn editor_scrollbar_tracks_remain_visible_and_enable_for_overflow(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("sql-editor-vertical-scrollbar-track")
            .is_some()
    );
    assert!(
        cx.debug_bounds("sql-editor-horizontal-scrollbar-track")
            .is_some()
    );
    assert!(
        cx.debug_bounds("sql-editor-vertical-scrollbar-thumb")
            .is_none()
    );
    assert!(
        cx.debug_bounds("sql-editor-horizontal-scrollbar-thumb")
            .is_none()
    );

    let long_line = "x".repeat(2_000);
    let overflowing = std::iter::repeat_n(long_line, 500)
        .collect::<Vec<_>>()
        .join("\n");
    view.update(cx, |editor, cx| editor.set_text(overflowing, cx));
    cx.run_until_parked();

    // The frame drawn by set_text computes the scroll extents during its
    // prepaint; the content must overflow on both axes or the editor
    // cannot scroll at all.
    view.read_with(cx, |editor, _| {
        let max = editor.scroll_handle.max_offset();
        assert!(max.y > px(0.), "content should overflow vertically");
        assert!(max.x > px(0.), "content should overflow horizontally");
    });

    // The thumbs read those extents at render time, so they appear on the
    // following frame.
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("sql-editor-vertical-scrollbar-thumb")
            .is_some()
    );
    assert!(
        cx.debug_bounds("sql-editor-horizontal-scrollbar-thumb")
            .is_some()
    );
}

#[test]
fn pivot_directive_completion_inserts_valid_templates() {
    let sql = "-- ";
    let (items, replace_start) = pivot_directive_completions(sql, sql.len()).unwrap();

    assert_eq!(replace_start, 0);
    assert_eq!(
        items[0].insert_text,
        "-- sqlly view:pivot\n-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum"
    );
    assert_eq!(items[0].replace_span, Some((0, sql.len())));
}

#[test]
fn pivot_directive_completion_ignores_sql_code() {
    assert!(pivot_directive_completions("SELECT ", "SELECT ".len()).is_none());
}

#[test]
fn sqlly_directive_completion_uses_the_namespaced_prefix() {
    let sql = "-- sqlly ";
    let (items, _) = pivot_directive_completions(sql, sql.len()).unwrap();
    assert!(
        items
            .iter()
            .all(|item| item.insert_text.starts_with("-- sqlly "))
    );
}

#[test]
fn directive_completions_accept_every_spelling_the_grammar_accepts() {
    // Trigger recognition rides the shared lexer, so loose-but-valid
    // spellings (no space, tab, any case) offer the same menus the
    // canonical spelling gets.
    for namespace_only in ["--sqlly", "--\tsqlly ", "-- SQLLY "] {
        let (items, _) = pivot_directive_completions(namespace_only, namespace_only.len()).unwrap();
        assert_eq!(items.len(), 5, "family menu for {namespace_only:?}");
    }
    for (line, expected_first) in [
        ("--sqlly view:", "-- sqlly view:pivot"),
        ("-- SQLLY PIVOT: row:", "-- sqlly pivot: row:Region,Country"),
    ] {
        let (items, _) = pivot_directive_completions(line, line.len()).unwrap();
        assert!(
            items[0].insert_text.starts_with(expected_first),
            "{line:?} → {:?}",
            items[0].insert_text
        );
    }
    // A completed name/canvas/placement directive offers no menu.
    for done in ["-- sqlly name 1: X", "-- sqlly 2x2", "-- sqlly result: 1|1"] {
        assert!(pivot_directive_completions(done, done.len()).is_none());
    }
}

#[test]
fn wildcard_and_identifier_helpers_preserve_qualified_targets() {
    assert_eq!(
        wildcard_at("SELECT o.* FROM Orders o", 9),
        Some((7..10, Some("o".into())))
    );
    assert_eq!(sql_identifier("Order Total"), "[Order Total]");
    assert_eq!(sql_identifier("OrderId"), "OrderId");
}

#[test]
fn qualified_wildcard_uses_the_tables_existing_alias() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT Orders.*\nFROM dbo.Orders o";
    let actions = editor_context_actions(sql, sql.find('*').unwrap(), None, &snapshot);
    let EditorContextAction::ExpandWildcard { sql, .. } = &actions[0] else {
        panic!("expected wildcard action");
    };
    assert_eq!(sql, "SELECT o.Id,\n    o.CustomerId\nFROM dbo.Orders o");
}

#[test]
fn bare_wildcard_uses_the_single_tables_existing_alias() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT *\nFROM dbo.Orders o";
    let actions = editor_context_actions(sql, sql.find('*').unwrap(), None, &snapshot);
    let EditorContextAction::ExpandWildcard { sql, .. } = &actions[0] else {
        panic!("expected wildcard action");
    };
    assert_eq!(sql, "SELECT o.Id,\n    o.CustomerId\nFROM dbo.Orders o");
}

#[test]
fn bare_wildcard_qualifies_every_table_and_creates_missing_aliases() {
    let snapshot = schema_snapshot(&[
        ("Orders", &["Id", "CustomerId"]),
        ("Customers", &["Id", "Name"]),
    ]);
    let sql = "SELECT *\nFROM dbo.Orders\nJOIN dbo.Customers c ON c.Id = Orders.CustomerId";
    let actions = editor_context_actions(sql, sql.find('*').unwrap(), None, &snapshot);
    let EditorContextAction::ExpandWildcard { sql, .. } = &actions[0] else {
        panic!("expected wildcard action");
    };
    assert_eq!(
        sql,
        "SELECT o.Id,\n    o.CustomerId,\n    c.Id,\n    c.Name\nFROM dbo.Orders o\nJOIN dbo.Customers c ON c.Id = o.CustomerId"
    );
}

#[test]
fn display_name_replaces_the_complete_qualified_foreign_key_expression() {
    use sqlly_lang::sqlmodel::{ForeignKeyDef, ObjectBody, SqlName};

    let mut orders = table_fragment("Orders", &["Id", "BusinessUnitId"]);
    let business_units = table_fragment("BusinessUnits", &["Id", "Name"]);
    let target = business_units.reference.clone();
    let dialect = orders.dialect;
    let ObjectBody::Table(table) = &mut orders.body else {
        unreachable!();
    };
    table.foreign_keys.push(ForeignKeyDef {
        name: None,
        columns: vec![SqlName::new("BusinessUnitId", dialect, false)],
        target: target.clone(),
        target_columns: vec![SqlName::new("Id", dialect, false)],
    });
    orders.references.push(target);
    let snapshot = snapshot_from_fragments(vec![orders, business_units]);
    let sql = "SELECT a.BusinessUnitId\nFROM dbo.Orders a";
    let actions = editor_context_actions(sql, sql.find("BusinessUnitId").unwrap(), None, &snapshot);
    let updated = actions
        .iter()
        .find_map(|action| match action {
            EditorContextAction::ShowDisplayName { sql } => Some(sql),
            _ => None,
        })
        .expect("display-name action");
    assert!(updated.starts_with("SELECT b.Name AS BusinessUnitId_Name\n"));
    assert!(!updated.contains("a.b.Name"));
    assert!(updated.contains("LEFT JOIN dbo.BusinessUnits b ON b.Id = a.BusinessUnitId"));
}

#[test]
fn qualify_offers_single_owner() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT CustomerId FROM dbo.Orders o";
    let offset = sql.find("CustomerId").unwrap();
    let actions = editor_context_actions(sql, offset, None, &snapshot);
    let EditorContextAction::QualifyIdentifier {
        sql: new_sql,
        cursor,
        qualifier,
        column,
    } = &actions[0]
    else {
        panic!("expected QualifyIdentifier, got {actions:?}");
    };
    assert_eq!(new_sql, "SELECT o.CustomerId FROM dbo.Orders o");
    assert_eq!(qualifier, "o");
    assert_eq!(column, "CustomerId");
    assert_eq!(*cursor, new_sql.find(" FROM").unwrap());
}

#[test]
fn qualify_offers_one_action_per_ambiguous_owner() {
    let snapshot = schema_snapshot(&[("Orders", &["Id"]), ("Customers", &["Id"])]);
    let sql = "SELECT Id FROM dbo.Orders o JOIN dbo.Customers c ON c.Id = o.CustomerId";
    let offset = sql.find("Id").unwrap();
    let actions = editor_context_actions(sql, offset, None, &snapshot);
    let qualify: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            EditorContextAction::QualifyIdentifier { qualifier, .. } => Some(qualifier.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(qualify, ["o", "c"]);
}

#[test]
fn qualify_uses_table_name_when_no_alias() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT Id FROM dbo.Orders";
    let offset = sql.find("Id").unwrap();
    let actions = editor_context_actions(sql, offset, None, &snapshot);
    let EditorContextAction::QualifyIdentifier { sql: new_sql, .. } = &actions[0] else {
        panic!("expected QualifyIdentifier, got {actions:?}");
    };
    assert_eq!(new_sql, "SELECT Orders.Id FROM dbo.Orders");
}

#[test]
fn qualify_brackets_aliases_that_need_it() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT Id FROM dbo.Orders AS [order line]";
    let offset = sql.find("Id").unwrap();
    let actions = editor_context_actions(sql, offset, None, &snapshot);
    let EditorContextAction::QualifyIdentifier {
        sql: new_sql,
        qualifier,
        ..
    } = &actions[0]
    else {
        panic!("expected QualifyIdentifier, got {actions:?}");
    };
    assert_eq!(qualifier, "[order line]");
    assert_eq!(
        new_sql,
        "SELECT [order line].Id FROM dbo.Orders AS [order line]"
    );
}

#[test]
fn unqualify_removes_redundant_prefix() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT o.CustomerId FROM dbo.Orders o";
    let offset = sql.find("CustomerId").unwrap();
    let actions = editor_context_actions(sql, offset, None, &snapshot);
    let EditorContextAction::UnqualifyIdentifier { sql: new_sql, .. } = actions
        .iter()
        .find(|action| matches!(action, EditorContextAction::UnqualifyIdentifier { .. }))
        .expect("unqualify action")
    else {
        unreachable!();
    };
    assert_eq!(new_sql, "SELECT CustomerId FROM dbo.Orders o");
}

#[test]
fn unqualify_refuses_ambiguous_column() {
    let snapshot = schema_snapshot(&[("Orders", &["Id"]), ("Customers", &["Id"])]);
    let sql = "SELECT o.Id FROM dbo.Orders o JOIN dbo.Customers c ON c.Id = o.Id";
    let offset = sql.find("Id").unwrap();
    let actions = editor_context_actions(sql, offset, None, &snapshot);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, EditorContextAction::UnqualifyIdentifier { .. }))
    );
}

#[test]
fn qualify_all_in_selection_qualifies_only_single_owner_columns() {
    let snapshot = schema_snapshot(&[
        ("Orders", &["Id", "CustomerId"]),
        ("Customers", &["Id", "Name"]),
    ]);
    let sql = "SELECT Id, CustomerId, Name, COUNT(Name), 'Name' FROM dbo.Orders o JOIN dbo.Customers c ON c.Id = o.CustomerId";
    let from = sql.find(" FROM").unwrap();
    let actions = editor_context_actions(sql, 0, Some(0..from), &snapshot);
    let EditorContextAction::QualifyAllInSelection {
        sql: new_sql,
        count,
    } = actions
        .iter()
        .find(|action| matches!(action, EditorContextAction::QualifyAllInSelection { .. }))
        .expect("qualify-all action")
    else {
        unreachable!();
    };
    assert_eq!(*count, 3);
    assert!(new_sql.starts_with("SELECT Id, o.CustomerId, c.Name, COUNT(c.Name), 'Name'"));
    assert!(!new_sql.contains("o.Id") && !new_sql.contains("c.Id,"));
}

#[test]
fn qualify_all_returns_none_when_nothing_changes() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT o.CustomerId FROM dbo.Orders o";
    let actions = editor_context_actions(sql, 0, Some(0..sql.len()), &snapshot);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, EditorContextAction::QualifyAllInSelection { .. }))
    );
}

#[test]
fn context_actions_still_expand_wildcards_with_selection_argument() {
    let snapshot = schema_snapshot(&[("Orders", &["Id", "CustomerId"])]);
    let sql = "SELECT *\nFROM dbo.Orders o";
    let actions = editor_context_actions(sql, sql.find('*').unwrap(), None, &snapshot);
    let EditorContextAction::ExpandWildcard { sql, .. } = &actions[0] else {
        panic!("expected wildcard action");
    };
    assert_eq!(sql, "SELECT o.Id,\n    o.CustomerId\nFROM dbo.Orders o");
}

#[test]
fn join_is_inserted_before_query_filters() {
    let sql = "SELECT o.CustomerId\nFROM Orders o\nWHERE o.Id > 1";
    let updated = insert_join_clause(sql, "LEFT JOIN Customers c ON c.Id = o.CustomerId");
    assert_eq!(
        updated,
        "SELECT o.CustomerId\nFROM Orders o\nLEFT JOIN Customers c ON c.Id = o.CustomerId\nWHERE o.Id > 1"
    );
}

fn plain_table_completion(label: &str, insert_text: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text: insert_text.to_string(),
        kind: CompletionKind::Table,
        source: CompletionSource::LiveMetadata,
        score: 100,
        detail: String::new(),
        replace_span: None,
    }
}

#[test]
fn auto_join_alias_is_added_only_when_plain_table_completion_is_accepted() {
    let preferences = EditorPreferences {
        alias_generation: AliasGeneration::AutoBasedOnJoin,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM dbo.Orders o JOIN Cus";
    let item = plain_table_completion("dbo.Customers", "dbo.Customers");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "dbo.Customers AS c"
    );

    let from_sql = "SELECT * FROM Cus";
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, from_sql, from_sql.len()),
        "dbo.Customers AS c"
    );

    let mut join_item = item.clone();
    join_item.kind = CompletionKind::Join;
    join_item.insert_text = "dbo.Customers c ON c.Id = o.CustomerId".to_string();
    assert_eq!(
        accepted_completion_insert_text(&join_item, &preferences, sql, sql.len()),
        join_item.insert_text
    );
}

/// Feedback ifb_9f91: a table accepted after a finished SELECT list writes
/// its FROM clause using the editor's indent unit, and no alias is added.
#[test]
fn from_clause_table_accept_uses_the_indent_preference() {
    let sql = "SELECT\n\t*\nf";
    let mut item = plain_table_completion("Forms.Form", "FROM\n\tForms.Form");
    item.detail = sqlly_intel::intellisense::FROM_CLAUSE_DETAIL.to_string();

    let spaces = EditorPreferences {
        indent_style: IndentStyle::Spaces,
        spaces_per_tab_raw: 2,
        alias_generation: AliasGeneration::CapitalLettersLowercase,
        auto_add_table_alias: true,
        ..EditorPreferences::default()
    };
    assert_eq!(
        accepted_completion_insert_text(&item, &spaces, sql, sql.len()),
        "FROM\n  Forms.Form"
    );

    let tabs = EditorPreferences {
        indent_style: IndentStyle::Tabs,
        ..EditorPreferences::default()
    };
    assert_eq!(
        accepted_completion_insert_text(&item, &tabs, sql, sql.len()),
        "FROM\n\tForms.Form"
    );
}

/// Feedback ifb_92d9: a column accepted after a FROM source writes its WHERE
/// clause using the editor's indent unit.
#[test]
fn where_clause_column_accept_uses_the_indent_preference() {
    let sql = "SELECT *\nFROM Shared.Settings s\nNa";
    let mut item = plain_table_completion("s.Name", "WHERE\n\ts.Name");
    item.kind = CompletionKind::Column;
    item.detail = sqlly_intel::intellisense::WHERE_CLAUSE_DETAIL.to_string();
    let spaces = EditorPreferences {
        indent_style: IndentStyle::Spaces,
        spaces_per_tab_raw: 4,
        ..EditorPreferences::default()
    };
    assert_eq!(
        accepted_completion_insert_text(&item, &spaces, sql, sql.len()),
        "WHERE\n    s.Name"
    );
}

#[test]
fn auto_join_alias_avoids_existing_aliases_deterministically() {
    let preferences = EditorPreferences {
        alias_generation: AliasGeneration::AutoBasedOnJoin,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM dbo.Companies c JOIN dbo.Countries c2 ON c2.Id = c.CountryId JOIN Cus";
    let item = plain_table_completion("dbo.Customers", "dbo.Customers");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "dbo.Customers AS c3"
    );
}

#[test]
fn capital_letters_alias_is_added_on_from_accept() {
    let preferences = EditorPreferences {
        alias_generation: AliasGeneration::CapitalLettersLowercase,
        auto_add_table_alias: true,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM ";
    let item = plain_table_completion("sales.SalesOrderHeader", "sales.SalesOrderHeader");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "sales.SalesOrderHeader AS soh"
    );
}

#[test]
fn capital_letters_alias_is_not_auto_inserted_when_preference_is_off() {
    let preferences = EditorPreferences {
        alias_generation: AliasGeneration::CapitalLettersLowercase,
        auto_add_table_alias: false,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM ";
    let item = plain_table_completion("sales.SalesOrderHeader", "sales.SalesOrderHeader");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "sales.SalesOrderHeader"
    );
}

/// With "Include the AS keyword" off, the alias follows the table bare.
#[test]
fn alias_is_written_without_as_when_the_keyword_is_off() {
    let preferences = EditorPreferences {
        alias_as_keyword: false,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM ";
    let item = plain_table_completion("PaymentMethod", "PaymentMethod");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "PaymentMethod pm"
    );
}

/// The default: accepting a table after FROM writes its alias with it.
#[test]
fn default_preferences_add_the_alias_on_accept() {
    let preferences = EditorPreferences::default();
    assert!(preferences.auto_add_table_alias);
    let sql = "SELECT * FROM ";
    let item = plain_table_completion("sales.SalesOrderHeader", "sales.SalesOrderHeader");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "sales.SalesOrderHeader AS soh"
    );
    assert_eq!(
        suggested_table_alias(&item, &preferences, sql, sql.len()),
        None,
        "an alias written on accept needs no follow-up suggestion"
    );
}

/// Second-acceptance mode: the accept inserts only the table, and the alias
/// is offered as the follow-up suggestion — for either alias rule.
#[test]
fn second_acceptance_mode_offers_the_alias_as_a_follow_up() {
    let sql = "SELECT * FROM dbo.Orders o JOIN ";
    let item = plain_table_completion("sales.SalesOrderHeader", "sales.SalesOrderHeader");
    for (rule, alias) in [
        (AliasGeneration::CapitalLettersLowercase, "soh"),
        (AliasGeneration::AutoBasedOnJoin, "s"),
    ] {
        let preferences = EditorPreferences {
            alias_generation: rule.clone(),
            auto_add_table_alias: false,
            ..EditorPreferences::default()
        };
        assert_eq!(
            accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
            "sales.SalesOrderHeader",
            "{rule:?}"
        );
        assert_eq!(
            suggested_table_alias(&item, &preferences, sql, sql.len()),
            Some((alias.to_string(), "SalesOrderHeader".to_string())),
            "{rule:?}"
        );
    }
    // "Do not suggest" offers nothing in either mode.
    let off = EditorPreferences {
        alias_generation: AliasGeneration::DoNotSuggest,
        auto_add_table_alias: false,
        ..EditorPreferences::default()
    };
    assert_eq!(suggested_table_alias(&item, &off, sql, sql.len()), None);
}

#[gpui::test]
fn accepting_a_table_adds_the_alias_or_offers_it_per_the_setting(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    // Automatic (default): one acceptance writes table and alias.
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT * FROM Pay", cx);
        editor.cursor = editor.content.len();
        editor.completions = std::rc::Rc::new(vec![plain_table_completion(
            "dbo.PaymentMethod",
            "dbo.PaymentMethod",
        )]);
        editor.completion_index = 0;
        editor.completion_visible = true;
        editor.completion_replace_start = "SELECT * FROM ".len();
        editor.accept_completion(cx);
        assert_eq!(
            editor.content.as_ref(),
            "SELECT * FROM dbo.PaymentMethod AS pm"
        );
        assert!(!editor.completion_visible, "no follow-up suggestion");
    });
    // Second acceptance: the first accept writes the table and offers the
    // alias; accepting that suggestion writes it.
    view.update(cx, |editor, cx| {
        editor.editor_prefs.auto_add_table_alias = false;
        editor.set_text("SELECT * FROM Pay", cx);
        editor.cursor = editor.content.len();
        editor.completions = std::rc::Rc::new(vec![plain_table_completion(
            "dbo.PaymentMethod",
            "dbo.PaymentMethod",
        )]);
        editor.completion_index = 0;
        editor.completion_visible = true;
        editor.completion_replace_start = "SELECT * FROM ".len();
        editor.accept_completion(cx);
        assert_eq!(editor.content.as_ref(), "SELECT * FROM dbo.PaymentMethod");
        assert!(editor.completion_visible, "the alias is offered next");
        assert_eq!(editor.completions[0].label, "pm");
        editor.accept_completion(cx);
        assert_eq!(
            editor.content.as_ref(),
            "SELECT * FROM dbo.PaymentMethod AS pm"
        );
    });
    // Second acceptance with "Include the AS keyword" off writes it bare.
    view.update(cx, |editor, cx| {
        editor.editor_prefs.alias_as_keyword = false;
        editor.set_text("SELECT * FROM Pay", cx);
        editor.cursor = editor.content.len();
        editor.completions = std::rc::Rc::new(vec![plain_table_completion(
            "dbo.PaymentMethod",
            "dbo.PaymentMethod",
        )]);
        editor.completion_index = 0;
        editor.completion_visible = true;
        editor.completion_replace_start = "SELECT * FROM ".len();
        editor.accept_completion(cx);
        editor.accept_completion(cx);
        assert_eq!(
            editor.content.as_ref(),
            "SELECT * FROM dbo.PaymentMethod pm"
        );
    });
}

#[gpui::test]
fn accepted_alias_does_not_reopen_completions_ifb_4629(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_services(Services::new(), cx);
        editor.set_text("SELECT\n\t*\nFROM\n\tAccounting.Account", cx);
        editor.cursor = editor.content.len();
        editor.completions = std::rc::Rc::new(vec![CompletionItem {
            label: "a".into(),
            insert_text: " AS a".into(),
            kind: CompletionKind::Snippet,
            source: CompletionSource::Snippet,
            score: 0,
            detail: String::new(),
            replace_span: Some((editor.cursor, editor.cursor)),
        }]);
        editor.completion_replace_start = editor.cursor;
        editor.set_completion_visible(true);
        editor.accept_completion(cx);
        assert_eq!(
            editor.text(),
            "SELECT\n\t*\nFROM\n\tAccounting.Account AS a"
        );
    });
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    view.read_with(cx, |editor, _| {
        assert!(!editor.completion_visible);
        assert_eq!(editor.last_completion_trigger, None);
    });

    view.update(cx, |editor, cx| editor.insert(" ", cx));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    view.read_with(cx, |editor, _| {
        assert_eq!(
            editor.last_completion_trigger,
            Some((IntelliSenseTriggerKind::AutomaticTyping, false)),
            "typing after acceptance resumes automatic completion"
        );
    });
}

#[gpui::test]
fn accepted_column_does_not_reopen_completions_ifb_0c92(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_services(Services::new(), cx);
        editor.set_text(
            "SELECT\n\t*\nFROM\n\tShared.UserGroup AS ug\nWHERE\n\tName = N'Estándar Programación de horarios'\n\tand ug.",
            cx,
        );
        editor.cursor = editor.content.len();
        editor.completions = std::rc::Rc::new(vec![CompletionItem {
            label: "ug.Description".into(),
            insert_text: "Description".into(),
            kind: CompletionKind::Column,
            source: CompletionSource::Inferred,
            score: 0,
            detail: String::new(),
            replace_span: Some((editor.cursor, editor.cursor)),
        }]);
        editor.completion_replace_start = editor.cursor;
        editor.set_completion_visible(true);
        editor.accept_completion(cx);
        assert!(editor.text().ends_with("and ug.Description"));
    });
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    view.read_with(cx, |editor, _| {
        assert!(!editor.completion_visible);
        assert_eq!(editor.last_completion_trigger, None);
    });

    view.update(cx, |editor, cx| editor.trigger_completions_manual(cx));
    cx.run_until_parked();
    view.read_with(cx, |editor, _| {
        assert_eq!(
            editor.last_completion_trigger,
            Some((IntelliSenseTriggerKind::ManualShortcut, true)),
            "manual completion still works at the accepted position"
        );
    });
}

/// A quoted table name keeps its quoting when the alias is added and when a
/// clause-writing insert is rebuilt with the user's indent unit.
#[test]
fn quoted_table_inserts_keep_their_quotes_through_accept() {
    let preferences = EditorPreferences::default();
    let sql = "SELECT * FROM Ord";
    let item = plain_table_completion("dbo.Order Details", "dbo.[Order Details]");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "dbo.[Order Details] AS od"
    );

    let sql = "SELECT\n\t*\nf";
    let mut from = plain_table_completion("dbo.Order Details", "FROM\n\tdbo.[Order Details]");
    from.detail = sqlly_intel::intellisense::FROM_CLAUSE_DETAIL.to_string();
    let spaces = EditorPreferences {
        indent_style: IndentStyle::Spaces,
        spaces_per_tab_raw: 2,
        ..EditorPreferences::default()
    };
    assert_eq!(
        accepted_completion_insert_text(&from, &spaces, sql, sql.len()),
        "FROM\n  dbo.[Order Details]"
    );
}

#[test]
fn alias_is_skipped_when_alias_already_follows() {
    let preferences = EditorPreferences {
        alias_generation: AliasGeneration::AutoBasedOnJoin,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM Cus c WHERE";
    let item = plain_table_completion("dbo.Customers", "dbo.Customers");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, 17),
        "dbo.Customers"
    );

    let sql_without_alias = "SELECT * FROM Cus WHERE";
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql_without_alias, 17),
        "dbo.Customers AS c"
    );
}

#[gpui::test]
fn accept_completion_records_table_not_keyword(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SEL", cx);
        editor.completions = std::rc::Rc::new(vec![
            plain_table_completion("dbo.Orders", "dbo.Orders"),
            CompletionItem {
                label: "SELECT".to_string(),
                insert_text: "SELECT".to_string(),
                kind: CompletionKind::Keyword,
                source: CompletionSource::Keyword,
                score: 10,
                detail: String::new(),
                replace_span: None,
            },
        ]);
        editor.completion_index = 0;
        editor.completion_visible = true;
        editor.completion_replace_start = 0;
        editor.accept_completion(cx);
        assert_eq!(editor.accept_history.lock().unwrap().len(), 1);
        editor.completions = std::rc::Rc::new(vec![CompletionItem {
            label: "SELECT".to_string(),
            insert_text: "SELECT".to_string(),
            kind: CompletionKind::Keyword,
            source: CompletionSource::Keyword,
            score: 10,
            detail: String::new(),
            replace_span: None,
        }]);
        editor.completion_index = 0;
        editor.completion_visible = true;
        editor.accept_completion(cx);
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.accept_history.lock().unwrap().len(), 1);
    });
}

#[test]
fn disabled_join_alias_preference_leaves_completion_unchanged() {
    let preferences = EditorPreferences {
        alias_generation: AliasGeneration::DoNotSuggest,
        ..EditorPreferences::default()
    };
    let sql = "SELECT * FROM dbo.Orders o JOIN Cus";
    let item = plain_table_completion("dbo.Customers", "dbo.Customers");
    assert_eq!(
        accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
        "dbo.Customers"
    );
}

#[test]
fn format_comment_updates_matching_column_by_name() {
    let sql = "-- sqlly format: column=Total; number=2; thousands=true\nSELECT Total";
    let updated = update_or_insert_format_comment(
        sql,
        "total",
        "-- sqlly format: column=Total; number=4; thousands=false",
    );
    assert_eq!(
        updated,
        "-- sqlly format: column=Total; number=4; thousands=false\nSELECT Total"
    );
}

#[test]
fn format_comment_replaces_spaced_variants_the_parser_honors() {
    // `column=Total ;` (space before the semicolon) is honored by
    // `query_format::parse`; the old prefix matcher missed it and
    // inserted a second rule above, which the stale line then overrode
    // (last rule wins). It must be replaced in place.
    let sql = "-- sqlly format: column=Total ; number=2\nSELECT Total";
    let updated = update_or_insert_format_comment(
        sql,
        "Total",
        "-- sqlly format: column=Total; number=4; thousands=false",
    );
    assert_eq!(
        updated,
        "-- sqlly format: column=Total; number=4; thousands=false\nSELECT Total"
    );
    // The result must parse to exactly the new rule.
    let hints = crate::query_format::parse(&updated);
    assert_eq!(hints.len(), 1);
    assert_eq!(
        hints[0].format,
        crate::query_format::QueryColumnFormat::Number {
            decimals: 4,
            thousands: false
        }
    );
}

#[test]
fn format_comment_round_trips_json_path_and_template_punctuation() {
    let sql = "SELECT payload, name";
    let json = update_or_insert_format_comment(
        sql,
        "payload",
        "-- sqlly format: column=payload; json=$.customer.name",
    );
    let json = update_or_insert_format_comment(
        &json,
        "name",
        "-- sqlly format: column=name; template=ORD-{value|pad:6}",
    );
    let hints = crate::query_format::parse(&json);
    assert_eq!(hints.len(), 2);
    assert_eq!(
        hints[0].format,
        crate::query_format::QueryColumnFormat::JsonPath {
            path: "$.customer.name".into(),
        }
    );
    assert_eq!(
        hints[1].format,
        crate::query_format::QueryColumnFormat::Template {
            template: "ORD-{value|pad:6}".into(),
        }
    );
}

#[test]
fn format_choices_follow_sql_type_class() {
    let date = column_format_choices("CreatedAt", sqlly_lang::sqlmodel::TypeClass::DateTime);
    assert!(date.iter().any(|choice| choice.directive == "__custom__"));
    assert!(
        date.iter()
            .filter(|choice| choice.directive != "__custom__")
            .all(|choice| choice.directive.contains("date="))
    );
    let binary = column_format_choices("Payload", sqlly_lang::sqlmodel::TypeClass::Binary);
    assert!(
        binary
            .iter()
            .any(|choice| choice.directive.ends_with("binary=hex"))
    );
    assert!(
        binary
            .iter()
            .any(|choice| choice.directive.ends_with("binary=base64"))
    );
    for format in ["binary=uuid", "binary=ulid"] {
        assert!(
            binary
                .iter()
                .any(|choice| choice.directive
                    == format!("-- sqlly format: column=Payload; {format}")),
            "{format} offered for binary columns"
        );
    }
    let integer = column_format_choices("Ts", sqlly_lang::sqlmodel::TypeClass::Integer);
    assert!(
        integer
            .iter()
            .any(|choice| choice.directive.ends_with("epoch=s"))
    );
    assert!(
        integer
            .iter()
            .any(|choice| choice.directive.ends_with("epoch=ms"))
    );
    let text = column_format_choices("Card", sqlly_lang::sqlmodel::TypeClass::Character);
    assert!(
        text.iter()
            .any(|choice| choice.directive.ends_with("mask=0/4"))
    );
    assert!(
        text.iter()
            .any(|choice| choice.directive.ends_with("mask=0/0"))
    );
}

#[test]
fn result_column_types_select_the_matching_format_dialog() {
    use sqlly_lang::sqlmodel::TypeClass;
    assert_eq!(
        result_column_type_class("datetimeoffset(7)"),
        TypeClass::DateTime
    );
    assert_eq!(
        result_column_type_class("decimal(18,2)"),
        TypeClass::Decimal
    );
    assert_eq!(result_column_type_class("bigint"), TypeClass::Integer);
    assert_eq!(result_column_type_class("bit"), TypeClass::Boolean);
    assert_eq!(
        result_column_type_class("varbinary(max)"),
        TypeClass::Binary
    );
}

#[gpui::test]
fn escape_closes_the_formatting_override_dialog(cx: &mut gpui::TestAppContext) {
    // The unified dialog chrome renders gpui-component Buttons, which read
    // the component theme global — install it first.
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT CreatedAt", cx);
        editor.open_result_column_format_dialog("CreatedAt".into(), "datetime2", None, cx);
    });
    cx.run_until_parked();
    assert!(view.read_with(cx, |editor, _| editor.format_dialog.is_some()));

    cx.simulate_keystrokes("escape");

    view.read_with(cx, |editor, _| {
        assert!(editor.format_dialog.is_none());
        assert_eq!(editor.text(), "SELECT CreatedAt");
    });
}

// ── Hardening: hostile inputs ──────────────────────────────────────────

#[gpui::test]
fn set_text_normalizes_crlf_and_snaps_cursor_to_boundary(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("SELECT 1\r\nFROM t\rWHERE 1=1", cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT 1\nFROM t\nWHERE 1=1");
    });
    // Shrink the content so the old caret byte offset falls inside a
    // multibyte char of the new content — must snap, not panic.
    view.update(cx, |e, cx| {
        e.cursor = 20;
        e.set_text("€€€€€€€", cx); // 21 bytes of 3-byte chars
    });
    view.update(cx, |e, cx| e.delete_backward(cx));
    view.read_with(cx, |e, _| {
        assert!(e.content.is_char_boundary(e.cursor));
    });
}

#[gpui::test]
fn replace_document_is_one_undo_step_and_preserves_prior_history(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    // Build some ordinary edit history by typing.
    view.update(cx, |e, cx| {
        e.set_text("", cx);
        e.insert("SELECT 1", cx);
    });
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 1"));

    // Restore-a-version style full replace.
    view.update(cx, |e, cx| e.replace_document("SELECT 2 FROM t", cx));
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 2 FROM t"));

    // A single Ctrl+Z brings back exactly the pre-replace text — the replace
    // did NOT wipe the undo stack (unlike set_text).
    view.update(cx, |e, cx| e.undo(cx));
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 1"));

    // Redo re-applies the replace as one step.
    view.update(cx, |e, cx| e.redo(cx));
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 2 FROM t"));
}

#[gpui::test]
fn replace_document_noop_on_identical_text_keeps_undo(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("", cx);
        e.insert("abc", cx);
    });
    // Replacing with identical content must not push a spurious undo entry…
    view.update(cx, |e, cx| e.replace_document("abc", cx));
    // …so undo reverts the original typing, not an empty no-op edit.
    view.update(cx, |e, cx| e.undo(cx));
    view.read_with(cx, |e, _| assert_eq!(e.text(), ""));
}

#[gpui::test]
fn replace_document_normalizes_crlf_and_snaps_cursor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT abcdefghij", cx);
        e.cursor = 17;
        e.replace_document("A\r\nB\rC", cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "A\nB\nC");
        assert!(e.content.is_char_boundary(e.cursor));
        assert!(e.cursor <= e.content.len());
    });
}

#[gpui::test]
fn paste_normalizes_crlf(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("", cx));
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("-- a\r\n-- b".to_string()));
    view.update_in(cx, |e, window, cx| {
        e.handle_paste(&Paste, window, cx);
    });
    view.read_with(cx, |e, _| assert_eq!(e.text(), "-- a\n-- b"));
}

// ── IME composition ────────────────────────────────────────────────────

#[gpui::test]
fn ime_composition_marks_commits_and_undoes_as_one_edit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT ", cx);
        e.cursor = 7;
    });
    // Two preedit updates ("n" then "ni"), as a Pinyin IME would send.
    view.update_in(cx, |e, window, cx| {
        e.replace_and_mark_text_in_range(None, "n", None, window, cx);
    });
    view.update_in(cx, |e, window, cx| {
        e.replace_and_mark_text_in_range(None, "ni", None, window, cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT ni");
        assert_eq!(e.marked_range, Some(7..9), "preedit span is marked");
    });
    // Commit replaces the whole preedit, not the empty selection.
    view.update_in(cx, |e, window, cx| {
        e.replace_text_in_range(None, "你", window, cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT 你");
        assert!(e.marked_range.is_none(), "commit ends the composition");
        assert_eq!(e.cursor, 7 + "你".len());
    });
    // The entire composition session is a single undo step.
    view.update(cx, |e, cx| e.undo(cx));
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT "));
}

#[gpui::test]
fn ime_unmark_keeps_text_as_committed(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("", cx));
    view.update_in(cx, |e, window, cx| {
        e.replace_and_mark_text_in_range(None, "é", None, window, cx);
    });
    view.update_in(cx, |e, window, cx| e.unmark_text(window, cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "é");
        assert!(e.marked_range.is_none());
    });
}

// ── LineIndex ───────────────────────────────────────────────────────────

#[test]
fn line_index_empty_string() {
    let idx = LineIndex::new("");
    assert_eq!(idx.line_count(), 1);
    assert_eq!(idx.line_start(0), 0);
    assert_eq!(idx.line_end("", 0), 0);
    assert_eq!(idx.line_col_for_offset(0), (0, 0));
}

#[test]
fn line_index_single_line() {
    let idx = LineIndex::new("SELECT 1");
    assert_eq!(idx.line_count(), 1);
    assert_eq!(idx.line_start(0), 0);
    assert_eq!(idx.line_end("SELECT 1", 0), 8);
    assert_eq!(idx.line_col_for_offset(0), (0, 0));
    assert_eq!(idx.line_col_for_offset(7), (0, 7));
    assert_eq!(idx.line_col_for_offset(8), (0, 8));
}

#[test]
fn line_index_multi_line() {
    let content = "SELECT *\nFROM users\nWHERE id = 1";
    let idx = LineIndex::new(content);
    assert_eq!(idx.line_count(), 3);
    assert_eq!(idx.line_start(0), 0);
    assert_eq!(idx.line_start(1), 9);
    assert_eq!(idx.line_start(2), 20);
    assert_eq!(idx.line_end(content, 0), 8);
    assert_eq!(idx.line_end(content, 1), 19);
    assert_eq!(idx.line_end(content, 2), 32);
}

#[test]
fn line_index_trailing_newline() {
    let content = "SELECT 1\n";
    let idx = LineIndex::new(content);
    assert_eq!(idx.line_count(), 2);
    assert_eq!(idx.line_start(0), 0);
    assert_eq!(idx.line_start(1), 9);
    assert_eq!(idx.line_end(content, 0), 8);
    assert_eq!(idx.line_end(content, 1), 9);
}

#[test]
fn line_index_offset_at_newline() {
    let content = "AB\nCD";
    let idx = LineIndex::new(content);
    assert_eq!(idx.line_col_for_offset(2), (0, 2));
    assert_eq!(idx.line_col_for_offset(3), (1, 0));
}

#[test]
fn line_index_offset_at_eof() {
    let content = "AB\nCD";
    let idx = LineIndex::new(content);
    assert_eq!(idx.line_col_for_offset(5), (1, 2));
}

#[test]
fn line_index_unicode() {
    let content = "SELECT \u{20ac}\nFROM t";
    let idx = LineIndex::new(content);
    assert_eq!(idx.line_count(), 2);
    assert_eq!(idx.line_start(0), 0);
    assert_eq!(idx.line_start(1), 11);
    assert_eq!(idx.line_col_for_offset(7), (0, 7));
    assert_eq!(idx.line_col_for_offset(11), (1, 0));
}

#[test]
fn line_index_tabs_preserved() {
    let content = "SELECT\t*\nFROM\tusers";
    let idx = LineIndex::new(content);
    assert_eq!(idx.line_count(), 2);
    assert_eq!(idx.line_start(0), 0);
    assert_eq!(idx.line_start(1), 9);
}

// ── visible_line_range ──────────────────────────────────────────────────

#[test]
fn visible_range_empty_content() {
    let r = visible_line_range(0, 20.0, 0.0, 600.0);
    assert_eq!(r, 0..0);
}

#[test]
fn visible_range_top_of_file() {
    let r = visible_line_range(10000, 20.0, 0.0, 600.0);
    assert_eq!(r.start, 0);
    assert!(r.end <= 40);
}

#[test]
fn visible_range_middle_scroll() {
    let r = visible_line_range(10000, 20.0, -10000.0, 600.0);
    assert!(r.start >= 500 - EDITOR_OVERSCAN_LINES);
    assert!(r.end <= 500 + 30 + EDITOR_OVERSCAN_LINES + 1);
}

#[test]
fn visible_range_bottom_scroll() {
    let r = visible_line_range(100, 20.0, -1880.0, 600.0);
    assert_eq!(r.end, 100);
}

#[test]
fn visible_range_zero_viewport_bounded() {
    let r = visible_line_range(50000, 20.0, 0.0, 0.0);
    assert!(r.end <= EDITOR_FALLBACK_VISIBLE_LINES);
    assert!(r.end > 0);
}

#[test]
fn visible_range_huge_line_count_bounded() {
    let r = visible_line_range(50000, 20.0, 0.0, 600.0);
    assert!(r.end - r.start <= 40);
}

#[test]
fn visible_range_overscan_present() {
    let r = visible_line_range(1000, 20.0, -400.0, 600.0);
    let center_start = (400.0_f32 / 20.0).floor() as usize;
    assert!(r.start <= center_start);
    assert!(r.start + EDITOR_OVERSCAN_LINES >= center_start || r.start == 0);
}

// ── SELECT column context detection ─────────────────────────────────────

#[test]
fn select_context_true_between_select_and_from() {
    assert!(in_select_column_context("SELECT col1, col2 ", 20));
}

#[test]
fn select_context_false_after_from() {
    assert!(!in_select_column_context("SELECT col1 FROM users", 25));
}

#[test]
fn select_context_false_no_select() {
    assert!(!in_select_column_context("INSERT INTO t VALUES (", 22));
}

#[test]
fn select_context_false_inside_parens_after_from() {
    // cursor inside parentheses that are part of the FROM clause
    assert!(!in_select_column_context("SELECT col1 FROM (SELECT ", 26));
}

#[test]
fn select_context_true_inside_function_call_in_column_list() {
    // paren depth 1 but the SELECT at depth 0 is still the enclosing one
    assert!(in_select_column_context("SELECT COALESCE(col1, ", 24));
}

#[test]
fn select_context_case_insensitive() {
    assert!(in_select_column_context("select col1, col2 ", 20));
    assert!(in_select_column_context("Select col1 ", 12));
}

#[test]
fn select_context_terminated_by_where() {
    assert!(!in_select_column_context("SELECT col1 WHERE col1 = 1", 27));
}

#[test]
fn select_context_nested_subquery_in_column() {
    // After the subquery closes, we're back in the outer SELECT column list
    let sql = "SELECT (SELECT 1), col2 ";
    assert!(in_select_column_context(sql, sql.len()));
}

// ── remove_one_indent_unit ──────────────────────────────────────────────

#[test]
fn remove_indent_unit_tabs() {
    assert_eq!(remove_one_indent_unit("\t\t", "\t"), "\t");
}

#[test]
fn remove_indent_unit_single_tab() {
    assert_eq!(remove_one_indent_unit("\t", "\t"), "");
}

#[test]
fn remove_indent_unit_spaces() {
    assert_eq!(remove_one_indent_unit("        ", "    "), "    ");
}

#[test]
fn remove_indent_unit_empty_is_empty() {
    assert_eq!(remove_one_indent_unit("", "\t"), "");
}

#[test]
fn remove_indent_unit_fallback_trailing_ws() {
    // "  \t" with unit "    " → removes trailing tab, leaving "  "
    assert_eq!(remove_one_indent_unit("  \t", "    "), "  ");
}

// ── unclosed_parens_before_cursor ───────────────────────────────────────

#[test]
fn unclosed_parens_zero_no_parens() {
    assert_eq!(unclosed_parens_before_cursor("SELECT col1", 11), 0);
}

#[test]
fn unclosed_parens_one_unclosed() {
    assert_eq!(unclosed_parens_before_cursor("COALESCE(", 9), 1);
}

#[test]
fn unclosed_parens_balanced() {
    assert_eq!(unclosed_parens_before_cursor("COUNT(*) ", 9), 0);
}

#[test]
fn unclosed_parens_nested() {
    assert_eq!(unclosed_parens_before_cursor("COALESCE(1, (", 13), 2);
}

#[test]
fn unclosed_parens_cross_line_unclosed() {
    // Parenthesis opened on a prior line, cursor on the next line — the
    // full-prefix scan must still see the unclosed `(`.
    assert_eq!(unclosed_parens_before_cursor("COALESCE(\n  col", 15), 1);
}

#[test]
fn unclosed_parens_cross_line_balanced() {
    // Parentheses opened and closed across lines — net depth is 0.
    assert_eq!(
        unclosed_parens_before_cursor("COALESCE(\n  col1,\n  col2\n)", 27),
        0
    );
}

#[test]
fn gutter_width_visible_reserves_space() {
    assert_eq!(SqlEditor::gutter_width(true), px(54.0));
}

#[test]
fn gutter_fits_four_to_seven_digit_line_numbers() {
    let digit = px(14.0);
    let width = |digits: f32| px(14.0 + 8.0 + 14.0) + digit * digits;
    // Short scripts still reserve four digits, so the gutter is steady.
    assert_eq!(SqlEditor::fitted_gutter_width(true, 1, digit), width(4.0));
    assert_eq!(
        SqlEditor::fitted_gutter_width(true, 9_999, digit),
        width(4.0)
    );
    // Then it grows a digit at a time…
    assert_eq!(
        SqlEditor::fitted_gutter_width(true, 10_000, digit),
        width(5.0)
    );
    assert_eq!(
        SqlEditor::fitted_gutter_width(true, 999_999, digit),
        width(6.0)
    );
    assert_eq!(
        SqlEditor::fitted_gutter_width(true, 9_999_999, digit),
        width(7.0)
    );
    // …and stops at seven.
    assert_eq!(
        SqlEditor::fitted_gutter_width(true, 50_000_000, digit),
        width(7.0)
    );
    assert_eq!(SqlEditor::fitted_gutter_width(false, 3834, digit), px(0.0));
}

#[test]
fn gutter_width_hidden_collapses_to_zero() {
    // When "Show line numbers" is off, the gutter must reclaim its space
    // so editor text starts at the left edge.
    assert_eq!(SqlEditor::gutter_width(false), px(0.0));
}

#[gpui::test]
fn gutter_line_numbers_reuse_shaped_glyphs_across_paints(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT 1\nSELECT 2\nSELECT 3", cx);
    });
    cx.draw(
        gpui::Point::default(),
        gpui::size(px(800.0), px(600.0)),
        |_window, _cx| div().size_full().child(view.clone()),
    );
    let first = view.read_with(cx, |e, _| e.shape_cache.borrow().gutter.len());
    assert!(first > 0, "first paint should shape visible line numbers");
    cx.draw(
        gpui::Point::default(),
        gpui::size(px(800.0), px(600.0)),
        |_window, _cx| div().size_full().child(view.clone()),
    );
    let second = view.read_with(cx, |e, _| e.shape_cache.borrow().gutter.len());
    assert_eq!(
        first, second,
        "a second paint should reuse the same gutter shapes"
    );
    view.update(cx, |e, cx| {
        let family = e.font.family.to_string();
        e.set_font(&family, 22.0, cx);
    });
    cx.draw(
        gpui::Point::default(),
        gpui::size(px(800.0), px(600.0)),
        |_window, _cx| div().size_full().child(view.clone()),
    );
    let after_zoom = view.read_with(cx, |e, _| e.shape_cache.borrow().gutter.len());
    assert!(
        after_zoom > 0,
        "font-size change must reshape gutter numbers, not leave them blank"
    );
}

#[test]
fn indent_unit_tabs_is_a_tab() {
    let prefs = EditorPreferences {
        indent_style: IndentStyle::Tabs,
        ..EditorPreferences::default()
    };
    // insert_tab inserts exactly this string; tab style -> single tab.
    assert_eq!(prefs.indent_unit(), "\t");
}

#[test]
fn indent_unit_spaces_uses_configured_width() {
    let prefs = EditorPreferences {
        indent_style: IndentStyle::Spaces,
        spaces_per_tab_raw: 3,
        ..EditorPreferences::default()
    };
    // insert_tab inserts exactly this string; spaces style -> N spaces.
    assert_eq!(prefs.indent_unit(), "   ");
}

// ── auto-indent keyword detection ──────────────────────────────────────

#[test]
fn indent_keyword_single_word_clauses() {
    assert!(ends_with_indent_keyword("SELECT"));
    assert!(ends_with_indent_keyword("FROM"));
    assert!(ends_with_indent_keyword("WHERE"));
    assert!(ends_with_indent_keyword("HAVING"));
    assert!(ends_with_indent_keyword("JOIN"));
    assert!(ends_with_indent_keyword("ON"));
    assert!(ends_with_indent_keyword("INTO"));
    assert!(ends_with_indent_keyword("VALUES"));
    assert!(ends_with_indent_keyword("SET"));
    assert!(ends_with_indent_keyword("CASE"));
    assert!(ends_with_indent_keyword("WHEN"));
    assert!(ends_with_indent_keyword("THEN"));
    assert!(ends_with_indent_keyword("ELSE"));
    assert!(ends_with_indent_keyword("BEGIN"));
    assert!(ends_with_indent_keyword("WITH"));
    assert!(ends_with_indent_keyword("INSERT"));
    assert!(ends_with_indent_keyword("UPDATE"));
    assert!(ends_with_indent_keyword("DELETE"));
    assert!(ends_with_indent_keyword("MERGE"));
}

#[test]
fn indent_keyword_case_insensitive() {
    assert!(ends_with_indent_keyword("select"));
    assert!(ends_with_indent_keyword("Select"));
    assert!(ends_with_indent_keyword("WHERE"));
    assert!(ends_with_indent_keyword("Where"));
    assert!(ends_with_indent_keyword("FrOm"));
}

#[test]
fn indent_keyword_multi_word_group_by_order_by() {
    assert!(ends_with_indent_keyword("GROUP BY"));
    assert!(ends_with_indent_keyword("ORDER BY"));
    assert!(ends_with_indent_keyword("PARTITION BY"));
    assert!(ends_with_indent_keyword("group by"));
    assert!(ends_with_indent_keyword("Order By"));
}

#[test]
fn indent_keyword_by_alone_does_not_trigger() {
    assert!(!ends_with_indent_keyword("BY"));
    assert!(!ends_with_indent_keyword("by"));
}

#[test]
fn indent_keyword_trailing_whitespace_ignored() {
    assert!(ends_with_indent_keyword("SELECT   "));
    assert!(ends_with_indent_keyword("FROM\t"));
    assert!(ends_with_indent_keyword("GROUP BY  "));
}

#[test]
fn indent_keyword_with_preceding_text() {
    assert!(ends_with_indent_keyword("* FROM Users WHERE"));
    assert!(ends_with_indent_keyword("1 FROM"));
    assert!(ends_with_indent_keyword("id = 1 AND x = 2 HAVING"));
}

#[test]
fn indent_keyword_non_keywords_do_not_trigger() {
    assert!(!ends_with_indent_keyword("AND"));
    assert!(!ends_with_indent_keyword("OR"));
    assert!(!ends_with_indent_keyword("NOT"));
    assert!(!ends_with_indent_keyword("AS"));
    assert!(!ends_with_indent_keyword("NULL"));
    assert!(!ends_with_indent_keyword("UNION"));
    assert!(!ends_with_indent_keyword("EXCEPT"));
    assert!(!ends_with_indent_keyword("INTERSECT"));
}

#[test]
fn indent_keyword_empty_and_whitespace() {
    assert!(!ends_with_indent_keyword(""));
    assert!(!ends_with_indent_keyword("   "));
    assert!(!ends_with_indent_keyword("\t\t"));
}

#[test]
fn indent_keyword_non_keyword_end_of_line() {
    assert!(!ends_with_indent_keyword("SELECT *"));
    assert!(!ends_with_indent_keyword("SELECT 1,"));
    assert!(!ends_with_indent_keyword("WHERE x = 5"));
    assert!(!ends_with_indent_keyword("FROM Users"));
    assert!(!ends_with_indent_keyword("ORDER BY name"));
    assert!(!ends_with_indent_keyword("GROUP BY dept"));
}

// ── word boundary scanning ─────────────────────────────────────────────

#[test]
fn word_boundary_right_lands_on_start_of_next_word() {
    let s = "hello world";
    assert_eq!(word_boundary_right(s, 0), 6); // start of "world"
    assert_eq!(word_boundary_right(s, 5), 6); // from the space -> start of "world"
    assert_eq!(word_boundary_right(s, 2), 6); // mid-word -> start of next word
    assert_eq!(word_boundary_right(s, 6), 11); // from "world" -> end (no next word)
}

#[test]
fn word_boundary_right_leading_whitespace() {
    assert_eq!(word_boundary_right("  hello", 0), 2); // start of "hello"
    assert_eq!(word_boundary_right("   ", 0), 3); // all whitespace -> end
}

#[test]
fn word_boundary_right_punctuation_is_its_own_chunk() {
    assert_eq!(word_boundary_right("foo.bar", 0), 3);
    assert_eq!(word_boundary_right("foo.bar", 3), 4); // the dot alone
    assert_eq!(word_boundary_right("foo.bar", 4), 7);
}

#[test]
fn word_boundary_is_snake_case_aware() {
    // Underscore is a separator, so "first_name" splits.
    assert_eq!(word_boundary_right("first_name", 0), 5);
    assert_eq!(word_boundary_right("first_name", 5), 6); // the underscore
    assert_eq!(word_boundary_right("first_name", 6), 10);
    assert_eq!(word_boundary_left("first_name", 10), 6);
    assert_eq!(word_boundary_left("first_name", 6), 5);
    assert_eq!(word_boundary_left("first_name", 5), 0);
}

#[test]
fn word_boundary_left_empty_and_all_whitespace() {
    assert_eq!(word_boundary_left("", 0), 0);
    assert_eq!(word_boundary_left("   ", 3), 0);
}

#[test]
fn word_boundary_clamps_past_end() {
    assert_eq!(word_boundary_right("abc", 100), 3);
    assert_eq!(word_boundary_left("abc", 100), 0);
}

// ── camel-hump boundary scanning ───────────────────────────────────────

#[test]
fn hump_boundary_right_camel_case() {
    assert_eq!(hump_boundary_right("fooBar", 0), 3); // before 'B'
    assert_eq!(hump_boundary_right("fooBar", 3), 6); // end of "Bar"
    assert_eq!(hump_boundary_right("fooBar", 4), 6); // mid "Bar" -> end
}

#[test]
fn hump_boundary_right_acronym() {
    // "URLParser" splits as "URL" | "Parser".
    assert_eq!(hump_boundary_right("URLParser", 0), 3);
    assert_eq!(hump_boundary_right("URLParser", 3), 9);
}

#[test]
fn hump_boundary_right_digit_boundary() {
    // "foo123Bar" -> "foo" | "123" | "Bar".
    assert_eq!(hump_boundary_right("foo123Bar", 0), 3);
    assert_eq!(hump_boundary_right("foo123Bar", 3), 6);
    assert_eq!(hump_boundary_right("foo123Bar", 6), 9);
}

#[test]
fn hump_boundary_left_camel_case() {
    assert_eq!(hump_boundary_left("fooBar", 6), 3); // start of "Bar"
    assert_eq!(hump_boundary_left("fooBar", 3), 0); // start of "foo"
    assert_eq!(hump_boundary_left("fooBar", 4), 3); // mid "Bar" -> its start
}

#[test]
fn hump_boundary_left_acronym() {
    assert_eq!(hump_boundary_left("URLParser", 9), 3); // start of "Parser"
    assert_eq!(hump_boundary_left("URLParser", 3), 0); // start of "URL"
    assert_eq!(hump_boundary_left("URLParser", 4), 3); // inside "Parser" -> its start
}

#[test]
fn hump_boundary_left_digit_boundary() {
    assert_eq!(hump_boundary_left("foo123Bar", 9), 6); // start of "Bar"
    assert_eq!(hump_boundary_left("foo123Bar", 6), 3); // start of "123"
    assert_eq!(hump_boundary_left("foo123Bar", 3), 0); // start of "foo"
}

#[test]
fn hump_boundary_skips_whitespace_and_punct() {
    assert_eq!(hump_boundary_left("foo bar", 7), 4); // start of "bar"
    assert_eq!(hump_boundary_right("foo.bar", 3), 4); // the dot is one hump
    assert_eq!(hump_boundary_right("foo  Bar", 0), 3); // "foo", then ws, then "Bar"
}

// ── caret-move selection arithmetic ────────────────────────────────────

#[test]
fn move_caret_select_seeds_anchor_at_old_cursor() {
    // No prior anchor, selecting -> anchor becomes old cursor, cursor moves.
    assert_eq!(move_caret(5, None, true, 2, 0), (2, Some(5)));
}

#[test]
fn move_caret_select_keeps_existing_anchor() {
    // Prior anchor preserved while selecting.
    assert_eq!(move_caret(5, Some(9), true, 2, 0), (2, Some(9)));
}

#[test]
fn move_caret_no_select_with_selection_collapses_to_edge() {
    // Leftward move collapses to the selection start; rightward to the end.
    assert_eq!(move_caret(5, Some(9), false, 2, 9), (9, None));
    assert_eq!(move_caret(5, Some(1), false, 8, 1), (1, None));
}

#[test]
fn move_caret_no_select_no_selection_moves_to_target() {
    assert_eq!(move_caret(5, None, false, 2, 5), (2, None));
}

// ── double-click token selection ───────────────────────────────────────

#[test]
fn token_range_at_selects_word() {
    assert_eq!(token_range_at("hello world", 0), (0, 5));
    assert_eq!(token_range_at("hello world", 2), (0, 5));
    assert_eq!(token_range_at("hello world", 6), (6, 11));
}

#[test]
fn token_range_at_selects_punctuation_run() {
    assert_eq!(token_range_at("a == b", 2), (2, 4)); // "==" run
    assert_eq!(token_range_at("foo.bar", 3), (3, 4)); // single dot
}

#[test]
fn token_range_at_selects_whitespace_run() {
    assert_eq!(token_range_at("a   b", 2), (1, 4)); // the 3-space run
}

#[test]
fn token_range_at_past_end_is_empty() {
    assert_eq!(token_range_at("abc", 99), (3, 3));
}

#[test]
fn token_range_at_mid_utf8_offset_does_not_panic() {
    // Byte 4 sits inside `é` (bytes 3..5). Byte 1 sits inside `日` (bytes 0..3).
    assert_eq!(token_range_at("café", 4), (0, 5));
    assert_eq!(token_range_at("日本語", 1), (0, 9));
}

#[test]
fn token_range_at_end_of_multibyte_buffer() {
    assert_eq!(token_range_at("café", 99), (5, 5));
}

#[test]
fn column_at_cursor_at_end_of_multibyte_buffer_does_not_panic() {
    let sql = "select café";
    // Offset 11 is inside `é`; sql.len() is the caret-at-end case that
    // used to clamp to len-1 and panic. Non-ASCII identifiers are not
    // recognized, so None is the correct non-panicking result.
    assert_eq!(column_at(sql, 11), None);
    assert_eq!(column_at(sql, sql.len()), None);
}

#[test]
fn column_at_mid_utf8_offset_does_not_panic() {
    assert_eq!(column_at("café x", 4), None);
}

// ── DDL navigation gesture decision ────────────────────────────────────

fn modifiers(platform: bool, control: bool, alt: bool, shift: bool) -> gpui::Modifiers {
    gpui::Modifiers {
        platform,
        control,
        alt,
        shift,
        function: false,
    }
}

#[gpui::test]
fn ddl_click_requires_configured_modifier(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_ddl_nav_prefs(
            crate::preferences::DdlNavigationPreferences {
                activation_modifier: crate::preferences::DdlNavigationModifier::Command,
                ..Default::default()
            },
            cx,
        );
    });
    view.read_with(cx, |e, _| {
        assert!(e.is_ddl_navigation_click(1, &modifiers(true, false, false, false)));
        // Wrong modifier, extra modifier, no modifier: all ignored.
        assert!(!e.is_ddl_navigation_click(1, &modifiers(false, true, false, false)));
        assert!(!e.is_ddl_navigation_click(1, &modifiers(true, false, false, true)));
        assert!(!e.is_ddl_navigation_click(1, &modifiers(false, false, false, false)));
        // Double/triple clicks keep their selection semantics.
        assert!(!e.is_ddl_navigation_click(2, &modifiers(true, false, false, false)));
        assert!(!e.is_ddl_navigation_click(3, &modifiers(true, false, false, false)));
    });
}

#[gpui::test]
fn ddl_click_disabled_pref_never_triggers(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_ddl_nav_prefs(
            crate::preferences::DdlNavigationPreferences {
                enabled: false,
                activation_modifier: crate::preferences::DdlNavigationModifier::Command,
                ..Default::default()
            },
            cx,
        );
    });
    view.read_with(cx, |e, _| {
        assert!(!e.is_ddl_navigation_click(1, &modifiers(true, false, false, false)));
    });
}

#[test]
fn restyle_link_span_splits_runs_and_preserves_total_len() {
    let font = gpui::font("monospace");
    let run = |len: usize| TextRun {
        len,
        font: font.clone(),
        color: Theme::text(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    // Three runs of 5 bytes each; link the middle span 4..11 (crosses
    // two run boundaries).
    let runs = restyle_link_span(vec![run(5), run(5), run(5)], 4, 11, &font);
    let total: usize = runs.iter().map(|r| r.len).sum();
    assert_eq!(total, 15, "restyling must not change covered length");
    // Underlined bytes must be exactly the link span.
    let underlined: usize = runs
        .iter()
        .filter(|r| r.underline.is_some())
        .map(|r| r.len)
        .sum();
    assert_eq!(underlined, 7);
    // First run's leading 4 bytes stay unstyled.
    assert_eq!(runs[0].len, 4);
    assert!(runs[0].underline.is_none());
}

#[test]
fn restyle_link_span_empty_span_is_identity() {
    let font = gpui::font("monospace");
    let runs = vec![TextRun {
        len: 10,
        font: font.clone(),
        color: Theme::text(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    let out = restyle_link_span(runs.clone(), 6, 6, &font);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len, 10);
}

#[gpui::test]
fn link_hover_clears_when_modifier_released(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_ddl_nav_prefs(
            crate::preferences::DdlNavigationPreferences {
                activation_modifier: crate::preferences::DdlNavigationModifier::Command,
                ..Default::default()
            },
            cx,
        );
        e.link_hover_range = Some((14, 23));
        e.update_link_hover(&modifiers(false, false, false, false), cx);
    });
    assert_eq!(
        view.read_with(cx, |e, _| e.link_hover_range),
        None,
        "releasing the modifier must clear the link affordance"
    );
}

#[gpui::test]
fn link_hover_ignores_wrong_modifier(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_ddl_nav_prefs(
            crate::preferences::DdlNavigationPreferences {
                activation_modifier: crate::preferences::DdlNavigationModifier::Command,
                ..Default::default()
            },
            cx,
        );
        e.link_hover_range = Some((14, 23));
        // Wrong modifier held → affordance clears rather than sticking.
        e.update_link_hover(&modifiers(false, false, true, false), cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.link_hover_range), None);
}

#[gpui::test]
fn ddl_click_rebinding_takes_effect(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_ddl_nav_prefs(
            crate::preferences::DdlNavigationPreferences {
                activation_modifier: crate::preferences::DdlNavigationModifier::Alt,
                ..Default::default()
            },
            cx,
        );
    });
    view.read_with(cx, |e, _| {
        assert!(e.is_ddl_navigation_click(1, &modifiers(false, false, true, false)));
        assert!(!e.is_ddl_navigation_click(1, &modifiers(true, false, false, false)));
    });
}

// ── integration: editor methods wire scanning to cursor/anchor state ───
//
// These exercise the `move_word_*` / `move_hump_*` / `move_doc_*` methods
// through a real `Context` to prove the helpers feed the editor state
// correctly (selection anchor seeding + cursor placement).

#[gpui::test]
fn move_word_left_right_through_editor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("hello world", cx));
    view.update(cx, |e, cx| {
        e.cursor = 11;
        e.move_word_left(false, cx);
    });
    let (cursor, anchor) = view.read_with(cx, |e, _| (e.cursor, e.selection_anchor));
    assert_eq!(cursor, 6, "word-left from end lands at start of 'world'");
    assert_eq!(anchor, None, "non-select move clears the anchor");

    view.update(cx, |e, cx| e.move_word_left(false, cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        0,
        "next word-left lands at start of 'hello'"
    );

    view.update(cx, |e, cx| e.move_word_right(false, cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        6,
        "word-right from start lands at start of 'world' (symmetric with word-left)"
    );
}

#[gpui::test]
fn select_word_seeds_and_extends_anchor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("hello world", cx));
    view.update(cx, |e, cx| {
        e.cursor = 9;
        e.move_word_left(true, cx); // select back to start of "world"
    });
    let (cursor, anchor) = view.read_with(cx, |e, _| (e.cursor, e.selection_anchor));
    assert_eq!(cursor, 6);
    assert_eq!(anchor, Some(9), "select seeds the anchor at the old cursor");
    let sel = view.read_with(cx, |e, _| e.selection());
    assert_eq!(sel, Some(6..9));
}

#[gpui::test]
fn vertical_move_lands_on_char_boundary_in_multibyte_text(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    // Line 0 contains a 3-byte char; a byte-column min() would land the
    // caret inside '€' and panic the next slice at the cursor.
    view.update(cx, |e, cx| e.set_text("€uro\nabcdef", cx));
    view.update(cx, |e, cx| {
        e.cursor = 9; // line 1, char col 2 ("ab|cdef")
        e.move_up(false, cx);
    });
    let cursor = view.read_with(cx, |e, _| e.cursor);
    assert!(
        view.read_with(cx, |e, _| e.content.is_char_boundary(cursor)),
        "vertical move must land on a char boundary (got byte {cursor})"
    );
    assert_eq!(cursor, 4, "char col 2 on '€uro' is before 'r' (byte 4)");
    // The historical crash: backspace at the landed offset sliced at a
    // non-boundary. Must not panic.
    view.update(cx, |e, cx| e.delete_backward(cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.content.to_string()),
        "€ro\nabcdef"
    );
}

#[gpui::test]
fn vertical_move_keeps_goal_column_through_short_line(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT column_name\nid\nFROM table_name", cx)
    });
    view.update(cx, |e, cx| {
        e.cursor = 10; // line 0, col 10
        e.move_down(false, cx);
    });
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        21,
        "clamped to the end of the short line 'id'"
    );
    view.update(cx, |e, cx| e.move_down(false, cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        32,
        "goal column restores to col 10 on the longer line below"
    );
    // A horizontal move forgets the goal column.
    view.update(cx, |e, cx| e.move_left(false, cx));
    view.update(cx, |e, cx| e.move_up(false, cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        21,
        "after a horizontal move the goal column resets to the live column"
    );
}

#[gpui::test]
fn move_hump_right_through_editor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("fooBarBaz", cx));
    view.update(cx, |e, cx| {
        e.cursor = 0;
        e.move_hump_right(false, cx);
    });
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        3,
        "hump-right lands before 'B'"
    );
    view.update(cx, |e, cx| e.move_hump_right(false, cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.cursor),
        6,
        "next hump-right lands before second 'B'"
    );
}

#[gpui::test]
fn move_doc_start_and_end(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("line1\nline2\nline3", cx));
    view.update(cx, |e, cx| {
        e.cursor = 12;
        e.move_doc_start(false, cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.cursor), 0);
    view.update(cx, |e, cx| e.move_doc_end(true, cx));
    let (cursor, anchor) = view.read_with(cx, |e, _| (e.cursor, e.selection_anchor));
    assert_eq!(cursor, 17);
    assert_eq!(anchor, Some(0), "select-to-end keeps the doc-start anchor");
}

#[gpui::test]
fn delete_word_backward_removes_one_word(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("hello world", cx);
        e.cursor = 11;
        e.delete_word_backward(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(text, "hello ");
    assert_eq!(view.read_with(cx, |e, _| e.cursor), 6);
}

#[gpui::test]
fn delete_to_line_end_clears_rest_of_line(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("select col1, col2", cx);
        e.cursor = 11; // after "col1"
        e.delete_to_line_end(cx);
    });
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "select col1"
    );
}

#[test]
fn completion_match_ranges_empty_filter() {
    assert!(completion_match_ranges("", "SELECT").is_empty());
}

#[test]
fn completion_match_ranges_prefix_match() {
    let ranges = completion_match_ranges("sel", "SELECT");
    assert_eq!(ranges, vec![(0, 3)]);
}

#[test]
fn completion_match_ranges_case_insensitive() {
    let ranges = completion_match_ranges("SEL", "select");
    assert_eq!(ranges, vec![(0, 3)]);

    let ranges = completion_match_ranges("sel", "SeLeCt");
    assert_eq!(ranges, vec![(0, 3)]);
}

#[test]
fn completion_match_ranges_subsequence_fuzzy() {
    // "SELECT" = S(0) E(1) L(2) E(3) C(4) T(5)
    let ranges = completion_match_ranges("slc", "SELECT");
    assert_eq!(ranges, vec![(0, 1), (2, 3), (4, 5)]);
}

#[test]
fn completion_match_ranges_contains_match() {
    let ranges = completion_match_ranges("ount", "COUNTY");
    assert_eq!(ranges, vec![(1, 5)]);
}

#[test]
fn completion_match_ranges_no_match() {
    let ranges = completion_match_ranges("xyz", "SELECT");
    assert!(ranges.is_empty());
}

#[test]
fn completion_match_ranges_filter_longer_than_label() {
    // "SELECT" is a subsequence of "SELECTALL", so it does match.
    // Use a filter that genuinely can't match instead.
    let ranges = completion_match_ranges("xyzabc", "SELECT");
    assert!(ranges.is_empty());
}

#[test]
fn completion_match_ranges_unicode() {
    let ranges = completion_match_ranges("café", "Café Table");
    assert_eq!(ranges, vec![(0, 5)]);
}

#[test]
fn completion_match_ranges_contiguous_run() {
    let ranges = completion_match_ranges("account", "Accounting");
    assert_eq!(ranges, vec![(0, 7)]);
}

#[test]
fn completion_match_ranges_dotted_qualified() {
    // "dbo.ord" matches "dbo.Ord" as a contiguous case-insensitive
    // subsequence — the dot doesn't break the range because it's
    // matched contiguously with the surrounding chars.
    let ranges = completion_match_ranges("dbo.ord", "dbo.Orders");
    assert_eq!(ranges, vec![(0, 7)]);
}

// ── completion popup scroll-into-view ──────────────────────────────────

#[test]
fn clamp_scroll_offset_no_change_when_in_view() {
    assert_eq!(clamp_scroll_offset(3, 0, 8), 0);
    assert_eq!(clamp_scroll_offset(7, 0, 8), 0);
    assert_eq!(clamp_scroll_offset(5, 2, 8), 2);
}

#[test]
fn clamp_scroll_offset_advances_when_past_bottom() {
    // index 8 with offset 0, max 8 → scroll so 8 is the last visible slot
    assert_eq!(clamp_scroll_offset(8, 0, 8), 1);
    assert_eq!(clamp_scroll_offset(15, 0, 8), 8);
    assert_eq!(clamp_scroll_offset(15, 5, 8), 8);
}

#[test]
fn clamp_scroll_offset_retreats_when_above_top() {
    // Scrolling up: index 3 with offset 5 → jump offset back to 3
    assert_eq!(clamp_scroll_offset(3, 5, 8), 3);
    assert_eq!(clamp_scroll_offset(0, 7, 8), 0);
}

#[test]
fn clamp_scroll_offset_unchanged_at_boundary() {
    // Last visible slot (index == offset + max - 1) stays put
    assert_eq!(clamp_scroll_offset(7, 0, 8), 0);
    // First visible slot (index == offset) stays put
    assert_eq!(clamp_scroll_offset(3, 3, 8), 3);
}

/// Every directive snippet the completion popup can insert must be a
/// directive the shared grammar accepts and the diagnostics pass stays
/// quiet on — the snippets and the parsers may never drift apart.
#[test]
fn directive_completion_snippets_survive_directive_diagnostics() {
    let triggers = [
        "-- sqlly ",
        "-- sqlly pivot:",
        "-- sqlly format:",
        "-- sqlly view:",
    ];
    for trigger in triggers {
        let (items, _) = pivot_directive_completions(trigger, trigger.len())
            .unwrap_or_else(|| panic!("trigger must offer completions: {trigger}"));
        assert!(!items.is_empty(), "trigger must offer items: {trigger}");
        for item in items {
            // Inserted at the top of a query, a snippet must never
            // squiggle (some snippets are multi-line; the whole insertion
            // is checked at once).
            let sql = format!("{}\nSELECT 1;", item.insert_text);
            let diags = crate::sqlly_directive::directive_diagnostics(&sql);
            assert!(
                diags.is_empty(),
                "snippet '{}' must produce no diagnostics: {diags:?}",
                item.insert_text
            );
        }
    }
}

// ── state-machine: move_completion_down scrolls offset ────────────────

fn make_test_completions(n: usize) -> Vec<CompletionItem> {
    (0..n)
        .map(|i| CompletionItem {
            label: format!("item_{i}"),
            insert_text: format!("item_{i}"),
            kind: CompletionKind::Keyword,
            source: CompletionSource::Builtin,
            score: 0,
            detail: String::new(),
            replace_span: None,
        })
        .collect()
}

// ── dot-segment completion (`.` / Right advance one path segment) ──────

fn dot_item(label: &str, kind: CompletionKind) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text: label.to_string(),
        kind,
        source: CompletionSource::LiveMetadata,
        score: 0,
        detail: String::new(),
        replace_span: None,
    }
}

#[test]
fn dot_advance_shared_segment_commits() {
    let items = vec![
        dot_item("Accounting", CompletionKind::Schema),
        dot_item("Accounting.Account", CompletionKind::Table),
        dot_item("Accounting.Address", CompletionKind::Table),
    ];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &[], "Acc"),
        Some("Accounting".to_string())
    );
}

#[test]
fn dot_advance_ambiguous_segments_do_not_guess() {
    let items = vec![
        dot_item("Accounting", CompletionKind::Schema),
        dot_item("Accounts", CompletionKind::Schema),
    ];
    assert_eq!(decide_dot_advance(&items, 0, false, &[], "Acc"), None);
}

#[test]
fn dot_advance_navigated_uses_selected_segment() {
    let items = vec![
        dot_item("Accounting", CompletionKind::Schema),
        dot_item("Accounts", CompletionKind::Schema),
    ];
    assert_eq!(
        decide_dot_advance(&items, 1, true, &[], "Acc"),
        Some("Accounts".to_string())
    );
}

#[test]
fn dot_advance_preserves_catalog_casing() {
    let items = vec![dot_item("Accounting", CompletionKind::Schema)];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &[], "acc"),
        Some("Accounting".to_string())
    );
}

#[test]
fn dot_advance_skips_leaf_single_segment_items() {
    // A lone column is a leaf: there is nothing below `AccountId.`.
    let items = vec![dot_item("AccountId", CompletionKind::Column)];
    assert_eq!(decide_dot_advance(&items, 0, false, &[], "Acc"), None);
    // But a leaf column must not block a container that shares the prefix.
    let items = vec![
        dot_item("AccountId", CompletionKind::Column),
        dot_item("Accounting", CompletionKind::Schema),
    ];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &[], "Acc"),
        Some("Accounting".to_string())
    );
}

#[test]
fn dot_advance_skips_keywords_snippets_and_values() {
    let items = vec![
        dot_item("Accounting", CompletionKind::Schema),
        dot_item("ACTION", CompletionKind::Keyword),
        dot_item("acc.snippet", CompletionKind::Snippet),
        dot_item("'Accounting'", CompletionKind::Value),
    ];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &[], "Acc"),
        Some("Accounting".to_string())
    );
}

#[test]
fn dot_advance_committed_path_walks_deeper() {
    let items = vec![
        dot_item("Accounting.Account", CompletionKind::Table),
        dot_item("Accounting.Account.OrderId", CompletionKind::Column),
        // A column of a table named like the head segment is a leaf here.
        dot_item("Accounting.AccountId", CompletionKind::Column),
    ];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &["Accounting"], "Acc"),
        Some("Account".to_string())
    );
}

#[test]
fn dot_advance_db_schema_suffix_alignment() {
    // `db.schema.` lists `schema.table` labels: the label aligns to the
    // committed path by suffix, not from the root.
    let items = vec![dot_item("Accounting.Account", CompletionKind::Table)];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &["MyDb", "Accounting"], "Acc"),
        Some("Account".to_string())
    );
}

#[test]
fn dot_advance_sideways_single_segment_label_is_ignored() {
    // An alias-like single-segment label does not continue a committed
    // path, so it must not propose a sideways hop or create ambiguity.
    let items = vec![
        dot_item("Accounting", CompletionKind::Table),
        dot_item("Accounting.Account", CompletionKind::Table),
    ];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &["Accounting"], "Acc"),
        Some("Account".to_string())
    );
}

#[test]
fn dot_advance_partial_must_prefix_next_segment() {
    let items = vec![dot_item("Accounting", CompletionKind::Schema)];
    assert_eq!(decide_dot_advance(&items, 0, false, &[], "zzz"), None);
}

#[test]
fn dot_advance_quoted_segments_are_trimmed() {
    let items = vec![dot_item("[Accounting].[Account]", CompletionKind::Table)];
    assert_eq!(
        decide_dot_advance(&items, 0, false, &[], "Acc"),
        Some("Accounting".to_string())
    );
}

/// Seed an editor with `sql`, the caret at the end, and a visible popup
/// holding `items`.
fn seed_dot_completion(
    editor: &mut SqlEditor,
    sql: &str,
    items: Vec<CompletionItem>,
    cx: &mut Context<SqlEditor>,
) {
    editor.set_text(sql, cx);
    editor.cursor = sql.len();
    editor.completions = std::rc::Rc::new(items);
    editor.completion_index = 0;
    editor.completion_scroll_offset = 0;
    editor.completion_replace_start = word_start_before(&editor.content, editor.cursor);
    editor.completion_user_navigated = false;
    editor.set_completion_visible(true);
}

#[gpui::test]
fn dot_key_advances_completion_segment(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![
                dot_item("Accounting", CompletionKind::Schema),
                dot_item("Accounting.Account", CompletionKind::Table),
            ],
            cx,
        );
    });
    cx.simulate_input(".");
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Accounting.");
        assert_eq!(editor.cursor(), "SELECT * FROM Accounting.".len());
    });
}

#[gpui::test]
fn right_arrow_advances_completion_segment(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![dot_item("Accounting", CompletionKind::Schema)],
            cx,
        );
    });
    cx.dispatch_action(Right);
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Accounting.");
        assert_eq!(editor.cursor(), "SELECT * FROM Accounting.".len());
    });
}

#[gpui::test]
fn right_arrow_without_popup_moves_caret(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT 1", cx);
        editor.cursor = 0;
    });
    cx.dispatch_action(Right);
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT 1");
        assert_eq!(editor.cursor(), 1);
    });
}

#[gpui::test]
fn right_arrow_with_ambiguous_segments_moves_caret(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![
                dot_item("Accounting", CompletionKind::Schema),
                dot_item("Accounts", CompletionKind::Schema),
            ],
            cx,
        );
    });
    cx.dispatch_action(Right);
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Acc");
    });
}

#[gpui::test]
fn dot_key_with_ambiguous_segments_inserts_literal_dot(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![
                dot_item("Accounting", CompletionKind::Schema),
                dot_item("Accounts", CompletionKind::Schema),
            ],
            cx,
        );
    });
    cx.simulate_input(".");
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Acc.");
    });
}

#[gpui::test]
fn dot_key_without_eligible_completion_inserts_literal_dot(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![dot_item("ACTION", CompletionKind::Keyword)],
            cx,
        );
    });
    cx.simulate_input(".");
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Acc.");
    });
}

#[gpui::test]
fn dot_key_with_empty_partial_inserts_literal_dot(cx: &mut gpui::TestAppContext) {
    // Right after a committed dot (`Accounting.|`) the partial is empty, so
    // `.` must insert a literal dot rather than drilling into the only child
    // segment the user never started typing.
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Accounting.",
            vec![dot_item("Accounting.Account", CompletionKind::Table)],
            cx,
        );
    });
    cx.simulate_input(".");
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Accounting..");
    });
}

#[gpui::test]
fn right_arrow_with_empty_partial_moves_caret(cx: &mut gpui::TestAppContext) {
    // With the caret right after a committed dot and only an empty partial,
    // Right must fall through to caret movement, not drill into a child.
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Accounting.x",
            vec![dot_item("Accounting.Account", CompletionKind::Table)],
            cx,
        );
        // Place the caret between the dot and the trailing `x` so the partial
        // left of the caret is empty but caret movement is observable.
        editor.cursor = "SELECT * FROM Accounting.".len();
    });
    cx.dispatch_action(Right);
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT * FROM Accounting.x");
        assert_eq!(editor.cursor(), "SELECT * FROM Accounting.x".len());
    });
}

#[gpui::test]
fn dot_advance_undo_reverts_segment_commit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![dot_item("Accounting", CompletionKind::Schema)],
            cx,
        );
        assert!(editor.try_advance_completion_to_dot(cx));
        assert_eq!(editor.text(), "SELECT * FROM Accounting.");
        editor.undo(cx);
        assert_eq!(editor.text(), "SELECT * FROM Acc");
    });
}

#[gpui::test]
fn dot_advance_records_dot_trigger_with_services(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_services(Services::new(), cx);
        seed_dot_completion(
            editor,
            "SELECT * FROM Acc",
            vec![dot_item("Accounting", CompletionKind::Schema)],
            cx,
        );
        assert!(editor.try_advance_completion_to_dot(cx));
        assert_eq!(editor.text(), "SELECT * FROM Accounting.");
        assert_eq!(
            editor.last_completion_trigger,
            Some((IntelliSenseTriggerKind::Dot, false))
        );
    });
}

#[gpui::test]
fn dot_advance_value_lookup_falls_through(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    let sql = "SELECT * FROM Accounting.Account WHERE Name = ";
    view.update(cx, |editor, cx| {
        editor.set_text(sql, cx);
        editor.cursor = sql.len();
        editor.active_value_lookup = Some(ValueContext {
            column: Some("Name".into()),
            table: Some("Account".into()),
            schema: Some("Accounting".into()),
            operator: sqlly_services::ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: sql.len(),
            replace_end: sql.len(),
            has_opening_quote: false,
        });
        editor.completions = std::rc::Rc::new(vec![dot_item("'Checking'", CompletionKind::Value)]);
        editor.set_completion_visible(true);
    });
    cx.simulate_input(".");
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), format!("{sql}."));
    });
}

#[gpui::test]
fn dot_advance_parameter_member_access_falls_through(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |editor, cx| {
        seed_dot_completion(
            editor,
            "SELECT @Acc",
            vec![dot_item("Accounting", CompletionKind::Schema)],
            cx,
        );
    });
    cx.simulate_input(".");
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT @Acc.");
    });
}

/// The popup's fade-in path (a fresh `completion_shown_at`) must paint:
/// the fade alpha multiplies into every popup color, so a regression here
/// would blank the popup for its first ~120ms. Drawing mid-fade exercises
/// the alpha < 1 branch — including its `request_animation_frame` — end
/// to end through the real `EditorElement` paint.
#[gpui::test]
fn completion_popup_paints_mid_fade(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, _| {
        e.content = "SEL".into();
        e.invalidate_text_derived_caches();
        e.cursor = 3;
        e.completions = std::rc::Rc::new(make_test_completions(3));
        e.completion_index = 0;
        e.completion_visible = true;
        e.completion_shown_at = Some(Instant::now());
    });
    // Paint through the view so `request_animation_frame` (fired while
    // the fade is in flight) has a rendering view to re-notify — the
    // same shape the element has in the real window tree.
    cx.draw(
        gpui::Point::default(),
        gpui::size(px(800.0), px(600.0)),
        |_window, _cx| div().size_full().child(view.clone()),
    );
    // The stamp survives the paint; steady-state (None) is covered by the
    // popup state-machine tests above, which paint fully opaque.
    assert!(view.read_with(cx, |e, _| e.completion_shown_at.is_some()));
}

/// `set_completion_visible` stamps the fade start only on the hidden →
/// shown edge: refreshing an already-open popup (typing narrows the
/// list) must not replay the fade.
#[gpui::test]
fn completion_fade_stamp_only_on_show_transition(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, _| {
        e.set_completion_visible(true);
        let first = e.completion_shown_at.expect("stamped on show");
        e.set_completion_visible(true);
        assert_eq!(
            e.completion_shown_at,
            Some(first),
            "visible -> visible keeps the original stamp"
        );
        e.set_completion_visible(false);
        e.set_completion_visible(true);
        assert!(
            e.completion_shown_at.expect("restamped") >= first,
            "hidden -> shown stamps afresh"
        );
    });
    let _ = cx;
}

#[gpui::test]
fn move_completion_down_advances_scroll_offset(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.completions = std::rc::Rc::new(make_test_completions(12));
        e.completion_visible = true;
        e.completion_index = 0;
        e.completion_scroll_offset = 0;
        // This test is about scrolling during navigation, so start from the
        // already-navigated state (a fresh popup opens unselected and the
        // first Down would land on item 0 instead of advancing).
        e.completion_user_navigated = true;
        let _ = cx;
    });

    // Move down through the first visible window — offset should stay 0
    // while index is within [0, COMPLETION_MAX_VISIBLE).
    for i in 0..COMPLETION_MAX_VISIBLE {
        view.update(cx, |e, cx| e.move_completion_down(cx));
        let (idx, off) =
            view.read_with(cx, |e, _| (e.completion_index, e.completion_scroll_offset));
        assert_eq!(idx, i + 1, "index should advance to {}", i + 1);
        // Offset stays 0 while index < max_visible
        if idx < COMPLETION_MAX_VISIBLE {
            assert_eq!(
                off, 0,
                "offset should stay 0 while index {} is in view",
                idx
            );
        }
    }

    // After the 8th move_down, index == 8 which is == max_visible, so the
    // offset should advance to 1 so index 8 is the last visible row.
    let (idx, off) = view.read_with(cx, |e, _| (e.completion_index, e.completion_scroll_offset));
    assert_eq!(idx, COMPLETION_MAX_VISIBLE);
    assert_eq!(
        off, 1,
        "offset should advance to 1 when index reaches max_visible"
    );

    // Keep moving down — offset should track index - max_visible + 1
    for i in (COMPLETION_MAX_VISIBLE + 1)..12 {
        view.update(cx, |e, cx| e.move_completion_down(cx));
        let (idx, off) =
            view.read_with(cx, |e, _| (e.completion_index, e.completion_scroll_offset));
        assert_eq!(idx, i);
        let expected_off = i.saturating_sub(COMPLETION_MAX_VISIBLE) + 1;
        assert_eq!(
            off, expected_off,
            "offset should be {} when index is {}",
            expected_off, i
        );
    }
}

#[gpui::test]
fn move_completion_up_retreats_scroll_offset(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    // Start at index 11, offset 4 (so the visible window is [4,11])
    view.update(cx, |e, cx| {
        e.completions = std::rc::Rc::new(make_test_completions(12));
        e.completion_visible = true;
        e.completion_index = 11;
        e.completion_scroll_offset = 4;
        // Scroll-behavior test: start already navigated (an unselected popup
        // would treat the first Up as "jump to the last item" instead).
        e.completion_user_navigated = true;
        let _ = cx;
    });

    // Move up within the visible window — offset should stay 4
    for i in (4..11).rev() {
        view.update(cx, |e, cx| e.move_completion_up(cx));
        let (idx, off) =
            view.read_with(cx, |e, _| (e.completion_index, e.completion_scroll_offset));
        assert_eq!(idx, i, "index should retreat to {}", i);
        if idx >= 4 {
            assert_eq!(
                off, 4,
                "offset should stay 4 while index {} is in view",
                idx
            );
        }
    }

    // Now index is 4 == offset, move up again → index 3 < offset 4
    // The clamp should retreat offset to 3.
    view.update(cx, |e, cx| e.move_completion_up(cx));
    let (idx, off) = view.read_with(cx, |e, _| (e.completion_index, e.completion_scroll_offset));
    assert_eq!(idx, 3);
    assert_eq!(
        off, 3,
        "offset should retreat to 3 when index goes above the visible window"
    );

    // Continue up to 0 — offset should follow
    for i in (0..3).rev() {
        view.update(cx, |e, cx| e.move_completion_up(cx));
        let (idx, off) =
            view.read_with(cx, |e, _| (e.completion_index, e.completion_scroll_offset));
        assert_eq!(idx, i);
        assert_eq!(off, i, "offset should be {} when index is {}", i, i);
    }
}

// ── Feature 1: multi-line Tab/Shift+Tab ────────────────────────────────

#[gpui::test]
fn multiline_tab_indents_each_line(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("line1\nline2\nline3", cx);
        // Select across all three lines: anchor at start, cursor at end.
        e.selection_anchor = Some(0);
        e.cursor = e.content.len();
        e.insert_tab(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(text, "\tline1\n\tline2\n\tline3");
}

#[gpui::test]
fn shift_tab_unindents_each_line(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("\tline1\n\tline2\n\tline3", cx);
        e.selection_anchor = Some(0);
        e.cursor = e.content.len();
        e.unindent(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(text, "line1\nline2\nline3");
}

#[gpui::test]
fn shift_tab_unindent_partial_indent(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("    line1\n    line2", cx);
        e.selection_anchor = Some(0);
        e.cursor = e.content.len();
        e.unindent(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    // Default indent unit is \t; with spaces present, removes up to 1 char.
    assert_eq!(text, "   line1\n   line2");
}

// ── SQL formatting ─────────────────────────────────────────────────────

#[gpui::test]
fn format_sql_reformats_whole_buffer(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let prefs = crate::preferences::SqlFormattingPreferences {
        keep_short_queries_on_one_line: false,
        ..Default::default()
    };
    view.update(cx, |e, cx| {
        e.set_text("select a, b from t", cx);
        e.format_sql_document_with(&prefs, cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert!(text.starts_with("SELECT"), "{text}");
    // Default `from_table_placement`: the table on its own indented line.
    assert!(text.contains("FROM\n"), "{text}");
    assert!(text.trim_end().ends_with('t'), "{text}");
}

#[gpui::test]
fn format_sql_is_a_single_undoable_edit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let prefs = crate::preferences::SqlFormattingPreferences {
        keep_short_queries_on_one_line: false,
        ..Default::default()
    };
    let original = "select a, b from t";
    view.update(cx, |e, cx| {
        e.set_text(original, cx);
        e.format_sql_document_with(&prefs, cx);
    });
    assert_ne!(view.read_with(cx, |e, _| e.text().to_string()), original);
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), original);
}

#[gpui::test]
fn format_sql_selection_only(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let prefs = crate::preferences::SqlFormattingPreferences {
        keep_short_queries_on_one_line: true,
        ..Default::default()
    };
    // Two statements; select only the second one.
    let text = "select 1 from a;\nselect   2   from b";
    let start = text.find("select   2").unwrap();
    view.update(cx, |e, cx| {
        e.set_text(text, cx);
        e.selection_anchor = Some(start);
        e.cursor = text.len();
        let range = e.selection().unwrap();
        e.format_sql_range(range, &prefs, cx);
    });
    let out = view.read_with(cx, |e, _| e.text().to_string());
    assert!(out.starts_with("select 1 from a;"), "{out}");
    assert!(out.contains("SELECT 2 FROM b"), "{out}");
}

// ── Feature 2: undo/redo ───────────────────────────────────────────────

#[gpui::test]
fn undo_reverts_insert(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("hello", cx);
        e.cursor = 5;
        e.replace_selection(" world", cx);
    });
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "hello world"
    );
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "hello");
}

#[gpui::test]
fn undo_coalesces_typing_into_word_groups(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("", cx));
    // Simulate typing "SELECT x" one char at a time, as GPUI delivers it.
    for ch in ["S", "E", "L", "E", "C", "T", " ", "x"] {
        view.update(cx, |e, cx| e.replace_selection(ch, cx));
    }
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SELECT x");
    // Word-granular undo: "x", then the space, then the whole "SELECT".
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SELECT ");
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SELECT");
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "");
}

#[gpui::test]
fn undo_coalesces_backspace_runs(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("abcdef", cx);
        e.cursor = 6;
    });
    for _ in 0..3 {
        view.update(cx, |e, cx| e.delete_backward(cx));
    }
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "abc");
    // One undo restores the whole contiguous backspace run.
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "abcdef");
}

#[gpui::test]
fn redo_reapplies_insert(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("hello", cx);
        e.cursor = 5;
        e.replace_selection(" world", cx);
    });
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "hello");
    view.update(cx, |e, cx| e.redo(cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "hello world"
    );
}

#[gpui::test]
fn undo_after_delete_restores_text(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("hello world", cx);
        e.cursor = 11;
        e.delete_word_backward(cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "hello ");
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "hello world"
    );
}

#[gpui::test]
fn undo_n_typed_groups_restores_original(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("go", cx));
    view.update(cx, |e, cx| {
        e.cursor = 2;
        e.replace_selection(" one", cx);
    });
    view.update(cx, |e, cx| e.replace_selection(" two", cx));
    view.update(cx, |e, cx| e.replace_selection(" three", cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "go one two three"
    );
    for _ in 0..3 {
        view.update(cx, |e, cx| e.undo(cx));
    }
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "go");
}

#[gpui::test]
fn undo_select_and_replace_restores_selection_text(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("select foo from t", cx);
        e.selection_anchor = Some(7);
        e.cursor = 10;
        e.replace_selection("bar", cx);
    });
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "select bar from t"
    );
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        "select foo from t"
    );
}

#[gpui::test]
fn undo_delete_backward_across_multibyte_char(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("café", cx);
        e.cursor = e.content.len();
        e.delete_backward(cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "caf");
    view.update(cx, |e, cx| e.undo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "café");
}

#[gpui::test]
fn redo_after_undo_replays_exactly(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("ab", cx);
        e.cursor = 2;
        e.replace_selection("cd", cx);
    });
    view.update(cx, |e, cx| e.undo(cx));
    view.update(cx, |e, cx| e.redo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "abcd");
    view.update(cx, |e, cx| e.undo(cx));
    view.update(cx, |e, cx| e.redo(cx));
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "abcd");
}

#[gpui::test]
fn undo_redo_fuzz_matches_shadow_string(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    const ORIGINAL: &str = "αx";
    view.update(cx, |e, cx| e.set_text(ORIGINAL, cx));
    let mut rng = 0xC0FFEE_u64;
    let next = |rng: &mut u64| {
        *rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        *rng
    };
    for _ in 0..250 {
        let op = next(&mut rng) % 4;
        view.update(cx, |e, cx| {
            let len = e.content.len();
            e.cursor = e.cursor.min(len);
            if !e.content.is_char_boundary(e.cursor) {
                e.cursor = 0;
            }
            e.selection_anchor = None;
            match op {
                0 => {
                    let ch = char::from(b'a' + (next(&mut rng) % 26) as u8);
                    e.replace_selection(&ch.to_string(), cx);
                }
                1 => e.delete_backward(cx),
                2 => e.undo(cx),
                _ => e.redo(cx),
            }
        });
        let text = view.read_with(cx, |e, _| e.text().to_string());
        assert!(
            text.is_char_boundary(text.len()),
            "buffer must stay valid UTF-8"
        );
    }
    let after = view.read_with(cx, |e, _| e.text().to_string());
    for _ in 0..400 {
        let before = view.read_with(cx, |e, _| e.text().to_string());
        view.update(cx, |e, cx| e.undo(cx));
        if view.read_with(cx, |e, _| e.text().to_string()) == before {
            break;
        }
    }
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        ORIGINAL,
        "undoing fully must restore the seed"
    );
    for _ in 0..400 {
        let before = view.read_with(cx, |e, _| e.text().to_string());
        view.update(cx, |e, cx| e.redo(cx));
        if view.read_with(cx, |e, _| e.text().to_string()) == before {
            break;
        }
    }
    assert_eq!(
        view.read_with(cx, |e, _| e.text().to_string()),
        after,
        "redoing fully must replay the fuzzed buffer"
    );
}

// ── completion popup selection model ───────────────────────────────────
//
// The popup opens with NO row selected. Rationale: the popup auto-appears
// while the user is typing ordinary SQL, so Enter must keep meaning "insert
// a newline" (e.g. breaking a select list after `i.Name,`) unless the user
// has explicitly reached into the list with Up/Down. Tab remains the
// zero-navigation "take the top suggestion" key.

#[gpui::test]
fn enter_inserts_newline_when_nothing_is_selected(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SEL", cx);
        e.cursor = 3;
        let mut items = make_test_completions(1);
        items[0].insert_text = "SELECT".into();
        e.completions = std::rc::Rc::new(items);
        e.completion_visible = true;
        e.completion_replace_start = 0;
        // Popup up, but the user never pressed Up/Down: Enter is a newline,
        // not an accept — this is the "Enter after a trailing comma swallowed
        // my line break" fix.
        e.accept_suggestion_or_insert_newline(cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SEL\n");
    // The stale popup is dismissed rather than left floating over the new line.
    assert!(view.read_with(cx, |e, _| !e.completion_visible));
}

#[gpui::test]
fn enter_accepts_the_completion_selected_with_arrows(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SEL", cx);
        e.cursor = 3;
        let mut items = make_test_completions(1);
        items[0].insert_text = "SELECT".into();
        e.completions = std::rc::Rc::new(items);
        e.completion_visible = true;
        e.completion_replace_start = 0;
        // Down explicitly highlights the first row; only now may Enter
        // accept instead of inserting a newline.
        e.move_completion_down(cx);
        e.accept_suggestion_or_insert_newline(cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SELECT");
}

#[gpui::test]
fn tab_accepts_the_first_item_without_a_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SEL", cx);
        e.cursor = 3;
        let mut items = make_test_completions(2);
        items[0].insert_text = "SELECT".into();
        e.completions = std::rc::Rc::new(items);
        e.completion_visible = true;
        e.completion_replace_start = 0;
        // Tab (which routes to accept_completion) takes the top-ranked item
        // even though no row is highlighted — quick acceptance must not
        // require an arrow press first.
        assert!(!e.completion_user_navigated);
        e.accept_completion(cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SELECT");
}

#[gpui::test]
fn down_selects_the_first_item_before_advancing(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("it", cx);
        e.cursor = 2;
        e.completions = std::rc::Rc::new(make_test_completions(3));
        e.completion_visible = true;
        e.completion_replace_start = 0;
        // The popup opened unselected, so the first Down must land ON the
        // first item (not skip to the second), and the popup stays open.
        e.move_completion_down(cx);
        assert!(e.completion_user_navigated);
        assert_eq!(e.completion_index, 0);
        assert!(e.completion_visible);
        // Subsequent presses walk the list as before.
        e.move_completion_down(cx);
        assert_eq!(e.completion_index, 1);
    });
}

#[gpui::test]
fn up_from_unselected_jumps_to_the_last_item(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("it", cx);
        e.cursor = 2;
        e.completions = std::rc::Rc::new(make_test_completions(3));
        e.completion_visible = true;
        e.completion_replace_start = 0;
        // With no selection, Up enters the list from the bottom — the mirror
        // of Down entering from the top.
        e.move_completion_up(cx);
        assert!(e.completion_user_navigated);
        assert_eq!(e.completion_index, 2);
        assert!(e.completion_visible);
        // A second Up walks upward normally.
        e.move_completion_up(cx);
        assert_eq!(e.completion_index, 1);
    });
}

#[gpui::test]
fn typing_after_navigation_drops_back_to_unselected(cx: &mut gpui::TestAppContext) {
    // Uses the value-lookup path because its re-filter runs synchronously in
    // tests; the async automatic pass resets the flag the same way when a
    // fresh response lands. Filtering rebuilds the list, so any highlight
    // the user had refers to rows that may have moved — Enter must go back
    // to meaning "newline" until they navigate the NEW list.
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "SELECT * FROM Accounting.Account WHERE Name = ";
    view.update(cx, |editor, cx| {
        editor.set_text(sql, cx);
        editor.cursor = sql.len();
        editor.active_value_lookup = Some(ValueContext {
            column: Some("Name".into()),
            table: Some("Account".into()),
            schema: Some("Accounting".into()),
            operator: sqlly_services::ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: sql.len(),
            replace_end: sql.len(),
            has_opening_quote: false,
        });
        editor.value_lookup_token = 1;
        editor.value_lookup_pending = true;
        editor.apply_value_lookup_results(
            1,
            vec!["Checking".into(), "Savings".into(), "Cash".into()],
            cx,
        );
        editor.move_completion_down(cx);
        assert!(editor.completion_user_navigated);
        // Typing re-filters the list → selection state resets.
        editor.insert("C", cx);
        assert!(!editor.completion_user_navigated);
    });
}

#[gpui::test]
fn enter_accepts_fim_suggestion(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT ", cx);
        e.cursor = 7;
        e.show_fim_suggestion("*", cx);
        e.accept_suggestion_or_insert_newline(cx);
    });
    assert_eq!(view.read_with(cx, |e, _| e.text().to_string()), "SELECT *");
}

#[gpui::test]
fn trigger_ai_completion_emits_fim_trigger_event(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let events: std::rc::Rc<std::cell::RefCell<Vec<bool>>> = Default::default();
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &SqlEditorEvent, _cx| {
            if matches!(event, SqlEditorEvent::FimTriggerRequested) {
                seen.borrow_mut().push(true);
            }
        })
        .detach();
    });
    view.update(cx, |e, cx| {
        e.set_text("SELECT ", cx);
        e.cursor = 7;
        e.request_ai_completion(cx);
    });
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 1);
}

#[gpui::test]
fn folding_collapses_display_lines_and_movement_skips_them(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("BEGIN\nSELECT 1;\nSELECT 2;\nEND\nSELECT 9", cx);
        assert_eq!(e.display_line_count(), 5);
        e.toggle_fold_at_line(0, cx);
        // Region (0,3) hides lines 1..=3; visible: header + trailing line.
        assert_eq!(e.display_line_count(), 2);
        // Down from the header skips the hidden interior entirely.
        e.cursor = 0;
        e.move_down(false, cx);
        let (line, _) = e.line_col_for_offset(e.cursor);
        assert_eq!(line, 4);
        e.move_up(false, cx);
        let (line, _) = e.line_col_for_offset(e.cursor);
        assert_eq!(line, 0);
        // Toggling again restores the full buffer.
        e.toggle_fold_at_line(0, cx);
        assert_eq!(e.display_line_count(), 5);
    });
}

#[gpui::test]
fn reveal_cursor_folds_reopens_hidden_region(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("BEGIN\nSELECT 1;\nEND", cx);
        e.toggle_fold_at_line(0, cx);
        assert_eq!(e.display_line_count(), 1);
        // Jump the cursor into the hidden interior, as find navigation or
        // undo can; the prepaint invariant reopens the fold.
        e.cursor = e.line_start_offset(1);
        assert!(e.reveal_cursor_folds());
        assert_eq!(e.display_line_count(), 3);
    });
}

#[gpui::test]
fn fold_at_cursor_parks_cursor_on_header(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("BEGIN\nSELECT 1;\nEND", cx);
        e.cursor = e.line_start_offset(1) + 2;
        e.fold_at_cursor(cx);
        let (line, _) = e.line_col_for_offset(e.cursor);
        assert_eq!(line, 0);
        assert_eq!(e.display_line_count(), 1);
    });
}

#[gpui::test]
fn editing_below_a_fold_keeps_it_collapsed(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("BEGIN\nSELECT 1;\nEND\nSELECT 9", cx);
        e.toggle_fold_at_line(0, cx);
        assert_eq!(e.display_line_count(), 2);
        // Type at the very end of the buffer — below the fold.
        e.cursor = e.text().len();
        e.insert("9", cx);
        assert_eq!(e.display_line_count(), 2);
    });
}

#[gpui::test]
fn has_selection_reflects_anchor_state(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT 1", cx);
        e.cursor = 3;
    });
    assert!(!view.read_with(cx, |e, _| e.has_selection()));
    view.update(cx, |e, _| {
        e.selection_anchor = Some(0);
    });
    assert!(view.read_with(cx, |e, _| e.has_selection()));
    // A collapsed anchor (anchor == cursor) is not a selection.
    view.update(cx, |e, _| {
        e.selection_anchor = Some(3);
    });
    assert!(!view.read_with(cx, |e, _| e.has_selection()));
}

// ── Feature 4: Shift+Enter in SELECT context ───────────────────────────

#[gpui::test]
fn shift_enter_in_select_adds_comma(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT col1\nFROM t", cx);
        // Cursor at end of "col1" line (before \n).
        e.cursor = "SELECT col1".len();
        e.shift_enter(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(text, "SELECT col1,\nFROM t");
}

#[gpui::test]
fn shift_enter_outside_select_just_newline(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("FROM t1\nJOIN t2", cx);
        e.cursor = "FROM t1".len();
        e.shift_enter(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    // No SELECT context → regular newline.
    assert_eq!(text, "FROM t1\n\nJOIN t2");
}

#[gpui::test]
fn enter_in_select_outdents_column(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT\n    col1\nFROM t", cx);
        // Cursor at end of "    col1" (indented column line, no comma).
        e.cursor = "SELECT\n    col1".len();
        e.insert_newline(cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    // Enter in SELECT context without comma → new line is outdented one
    // level (from 4 spaces to 3, since default unit is \t = 1 char).
    assert!(
        text.contains("col1\n   \n"),
        "expected outdented new line after col1, got: {text}"
    );
}

// ── Feature 5: comma+space in SELECT context ───────────────────────────

#[gpui::test]
fn comma_then_space_in_select_creates_newline(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT col1\nFROM t", cx);
        // Type comma at end of col1 line.
        e.cursor = "SELECT col1".len();
        e.replace_selection(",", cx);
    });
    // Now type space — should trigger comma→newline expansion.
    view.update(cx, |e, cx| {
        e.cursor = "SELECT col1,".len();
        let applied = e.try_comma_space_expansion(" ", None, cx);
        assert!(applied, "comma-space expansion should have been applied");
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    // The next line (`FROM t`) is not a blank column slot, so a fresh line is
    // inserted for the new column and the caret lands on it, ready for the
    // (auto-opened) column suggestions.
    assert_eq!(text, "SELECT col1,\n\nFROM t");
    view.read_with(cx, |e, _| {
        assert_eq!(e.cursor, "SELECT col1,\n".len());
    });
}

#[gpui::test]
fn comma_then_space_inserts_indented_column_line_before_from(cx: &mut gpui::TestAppContext) {
    // The user's report: comma at the end of the last projected column, with an
    // aliased table and the FROM clause on the next line. Space should open a
    // fresh, correctly-indented column line rather than doing nothing.
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT\n    i.Name,\nFROM\n    Inventory.Item i", cx);
        e.cursor = "SELECT\n    i.Name,".len();
        let applied = e.try_comma_space_expansion(" ", None, cx);
        assert!(applied, "comma-space expansion should have been applied");
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(
        text,
        "SELECT\n    i.Name,\n    \nFROM\n    Inventory.Item i"
    );
    view.read_with(cx, |e, _| {
        // Caret sits at the end of the new indented line.
        assert_eq!(e.cursor, "SELECT\n    i.Name,\n    ".len());
    });
}

#[gpui::test]
fn comma_then_space_hops_onto_existing_blank_column_slot(cx: &mut gpui::TestAppContext) {
    // When the next line is already a blank, indented slot, don't add another
    // line — move the caret onto it (and open the popup).
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT\n    col1,\n    \n    col2\nFROM t", cx);
        e.cursor = "SELECT\n    col1,".len();
        let applied = e.try_comma_space_expansion(" ", None, cx);
        assert!(applied, "comma-space expansion should have been applied");
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    // Text is unchanged — no extra blank line inserted.
    assert_eq!(text, "SELECT\n    col1,\n    \n    col2\nFROM t");
    view.read_with(cx, |e, _| {
        assert_eq!(e.cursor, "SELECT\n    col1,\n    ".len());
    });
}

#[gpui::test]
fn automagic_select_comma_is_opt_in_and_one_undo_step(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT\n    first_name\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(!editor.try_automagic_comma_repair("l", None, cx));
        editor.editor_prefs.automagic_select_commas = true;
        assert!(editor.try_automagic_comma_repair("l", None, cx));
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT\n    first_name,\n    l");
    });
    view.update(cx, |editor, cx| editor.undo(cx));
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT\n    first_name\n    ");
    });
}

#[gpui::test]
fn automagic_connector_is_opt_in_and_one_undo_step(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT x FROM t\nWHERE a = 1\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(!editor.try_automagic_connector_repair("b", None, cx));
        editor.editor_prefs.automagic_where_connectors = true;
        assert!(editor.try_automagic_connector_repair("b", None, cx));
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT x FROM t\nWHERE a = 1\n    AND b");
    });
    view.update(cx, |editor, cx| editor.undo(cx));
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "SELECT x FROM t\nWHERE a = 1\n    ");
    });
}

#[gpui::test]
fn automagic_connector_covers_join_on_and_skips_incomplete_lines(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.editor_prefs.automagic_where_connectors = true;
        // JOIN … ON is a predicate context.
        editor.set_text("SELECT x FROM a\nJOIN b ON a.id = b.id\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(editor.try_automagic_connector_repair("a", None, cx));
        assert!(editor.text().contains("ON a.id = b.id\n    AND a"));

        // A previous line already ending in a connector never triggers.
        editor.set_text("SELECT x FROM t\nWHERE a = 1 AND\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(!editor.try_automagic_connector_repair("b", None, cx));

        // A pending BETWEEN waits for the user's own AND.
        editor.set_text("SELECT x FROM t\nWHERE a BETWEEN 1\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(!editor.try_automagic_connector_repair("b", None, cx));

        // SELECT lists are not predicate contexts.
        editor.set_text("SELECT\n    first_name\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(!editor.try_automagic_connector_repair("l", None, cx));

        // A dangling comparison never triggers.
        editor.set_text("SELECT x FROM t\nWHERE a =\n    ", cx);
        editor.cursor = editor.content.len();
        assert!(!editor.try_automagic_connector_repair("b", None, cx));
    });
}

#[test]
fn predicate_clause_scan_tracks_where_join_and_terminators() {
    assert!(predicate_clause_at("SELECT x FROM t WHERE a = 1\nb", 29).is_some());
    assert!(predicate_clause_at("SELECT x FROM a JOIN b ON a.id = b.id\nc", 39).is_some());
    // GROUP BY ends the predicate context.
    let sql = "SELECT x FROM t WHERE a = 1 GROUP BY x\ny";
    assert!(predicate_clause_at(sql, sql.len()).is_none());
    // ON without a JOIN (no context) does not count.
    let sql = "SELECT x FROM t\ny";
    assert!(predicate_clause_at(sql, sql.len()).is_none());
    // Strings and comments never open a predicate context.
    let sql = "SELECT '-- WHERE fake' FROM t\ny";
    assert!(predicate_clause_at(sql, sql.len()).is_none());
}

#[test]
fn predicate_line_completeness_rules() {
    assert!(predicate_line_is_incomplete("WHERE a = 1 AND"));
    assert!(predicate_line_is_incomplete("a LIKE"));
    assert!(predicate_line_is_incomplete("a BETWEEN 1"));
    assert!(predicate_line_is_incomplete("NOT"));
    assert!(!predicate_line_is_incomplete("a BETWEEN 1 AND 5"));
    assert!(!predicate_line_is_incomplete("a = 1"));
    assert!(!predicate_line_is_incomplete("b IS NOT NULL"));
}

#[gpui::test]
fn automagic_leading_comma_supports_group_and_order_lists(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT x FROM t\nGROUP BY\n    region\n    ", cx);
        editor.cursor = editor.content.len();
        editor.editor_prefs.automagic_group_by_commas = true;
        editor.editor_prefs.automagic_comma_placement =
            crate::preferences::AutomagicCommaPlacement::Leading;
        assert!(editor.try_automagic_comma_repair("c", None, cx));
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(
            editor.text(),
            "SELECT x FROM t\nGROUP BY\n    region\n    , c"
        );
    });
}

#[test]
fn automagic_clause_scan_ignores_comments_strings_and_quoted_names() {
    let sql = "SELECT '-- FROM fake', [order]\nvalue\n";
    assert_eq!(list_clause_at(sql, sql.len()), Some(ListClause::Select));
    let comment = "SELECT value\n-- GROUP BY\nnext\n";
    assert_eq!(
        list_clause_at(comment, comment.len()),
        Some(ListClause::Select)
    );
    let group = "SELECT x FROM t GROUP BY region\ncountry\n";
    assert_eq!(
        list_clause_at(group, group.len()),
        Some(ListClause::GroupBy)
    );
    let order = "SELECT x FROM t ORDER BY region\ncountry\n";
    assert_eq!(
        list_clause_at(order, order.len()),
        Some(ListClause::OrderBy)
    );
}

#[gpui::test]
fn comma_space_inside_multiline_function_does_not_trigger(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT COALESCE(\n  col1", cx);
        // Type comma after col1 inside the unclosed COALESCE(.
        e.cursor = "SELECT COALESCE(\n  col1".len();
        e.replace_selection(",", cx);
    });
    view.update(cx, |e, cx| {
        e.cursor = "SELECT COALESCE(\n  col1,".len();
        let applied = e.try_comma_space_expansion(" ", None, cx);
        assert!(
            !applied,
            "expansion should NOT trigger inside unclosed parens"
        );
    });
    // Since expansion was not applied, manually insert the space (as GPUI would).
    view.update(cx, |e, cx| {
        e.replace_selection(" ", cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    // Unclosed paren → no expansion, just a literal space.
    assert_eq!(text, "SELECT COALESCE(\n  col1, ");
}

#[gpui::test]
fn comma_space_outside_select_does_not_trigger(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("FROM t1\nJOIN t2", cx);
        e.cursor = "FROM t1".len();
        e.replace_selection(",", cx);
    });
    view.update(cx, |e, cx| {
        e.cursor = "FROM t1,".len();
        let applied = e.try_comma_space_expansion(" ", None, cx);
        assert!(
            !applied,
            "expansion should NOT trigger outside SELECT context"
        );
    });
    view.update(cx, |e, cx| {
        e.replace_selection(" ", cx);
    });
    let text = view.read_with(cx, |e, _| e.text().to_string());
    assert_eq!(text, "FROM t1, \nJOIN t2");
}

// ── Line verbs & Go to Line ────────────────────────────────────────────

#[gpui::test]
fn move_line_down_and_up_round_trips(cx: &mut gpui::TestAppContext) {
    let (view, cx) = find_bar_editor(cx);
    view.update(cx, |e, cx| {
        e.set_text("alpha\nbravo\ncharlie", cx);
        e.cursor = 0; // on "alpha"
        e.move_lines(false, cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "bravo\nalpha\ncharlie");
        let (line, col) = e.line_col_for_offset(e.cursor);
        assert_eq!((line, col), (1, 0), "cursor follows the moved line");
    });
    view.update(cx, |e, cx| e.move_lines(true, cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "alpha\nbravo\ncharlie");
        assert_eq!(e.cursor, 0);
    });
    // Moving the top line up / bottom line down is a no-op.
    view.update(cx, |e, cx| {
        e.cursor = 0;
        e.move_lines(true, cx);
        assert_eq!(e.text(), "alpha\nbravo\ncharlie");
        e.cursor = e.text().len();
        e.move_lines(false, cx);
        assert_eq!(e.text(), "alpha\nbravo\ncharlie");
    });
}

#[gpui::test]
fn move_lines_carries_a_multi_line_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = find_bar_editor(cx);
    view.update(cx, |e, cx| {
        e.set_text("one\ntwo\nthree\nfour", cx);
        // Select "two\nthree".
        e.selection_anchor = Some(4);
        e.cursor = 13;
        e.move_lines(false, cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "one\nfour\ntwo\nthree");
        assert_eq!(e.selection_anchor, Some(9));
        assert_eq!(e.cursor, 18);
    });
}

#[gpui::test]
fn duplicate_line_copies_below_and_moves_cursor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = find_bar_editor(cx);
    view.update(cx, |e, cx| {
        e.set_text("SELECT 1\nSELECT 2", cx);
        e.cursor = 3; // inside "SELECT 1"
        e.duplicate_line_or_selection(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT 1\nSELECT 1\nSELECT 2");
        let (line, col) = e.line_col_for_offset(e.cursor);
        assert_eq!((line, col), (1, 3), "cursor lands on the duplicate");
    });
}

#[gpui::test]
fn join_lines_trims_and_places_cursor_at_seam(cx: &mut gpui::TestAppContext) {
    let (view, cx) = find_bar_editor(cx);
    view.update(cx, |e, cx| {
        e.set_text("SELECT *  \n    FROM t\nWHERE x = 1", cx);
        e.cursor = 0;
        e.join_lines(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT * FROM t\nWHERE x = 1");
        assert_eq!(e.cursor, "SELECT * ".len());
        assert_eq!(e.selection_anchor, None);
    });
}

#[gpui::test]
fn go_to_line_jumps_and_clamps(cx: &mut gpui::TestAppContext) {
    let (view, cx) = find_bar_editor(cx);
    view.update(cx, |e, cx| {
        e.set_text("a\nbb\nccc", cx);
        e.go_to_line(2, cx);
    });
    view.read_with(cx, |e, _| {
        let (line, col) = e.line_col_for_offset(e.cursor);
        assert_eq!((line, col), (1, 0));
    });
    view.update(cx, |e, cx| e.go_to_line(999, cx));
    view.read_with(cx, |e, _| {
        let (line, _) = e.line_col_for_offset(e.cursor);
        assert_eq!(line, 2, "clamps to the last line");
    });
    view.update(cx, |e, cx| e.go_to_line(0, cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.cursor, 0, "line 0 clamps to line 1");
    });
}

// ── Find / Replace bar ─────────────────────────────────────────────────

/// gpui-component `Input` renders call native window APIs that gpui's
/// TestWindow does not implement (the macOS content-type sync panics), so
/// these tests keep the bar's overlay out of the element tree: the bar is
/// hidden inside the same update that opens it — every draw sees it
/// hidden — while the state machine (scan, navigation, replace, undo,
/// dismiss) is exercised for real. The visible overlay itself is manual
/// QA.
fn find_bar_editor(
    cx: &mut gpui::TestAppContext,
) -> (Entity<SqlEditor>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    cx.add_window_view(|_window, cx| SqlEditor::new(cx))
}

/// Open the bar, then hide it before this update's draw so the overlay
/// (and its `Input`s) never renders on a TestWindow.
fn open_hidden_find_bar(
    editor: &Entity<SqlEditor>,
    cx: &mut gpui::VisualTestContext,
    show_replace: bool,
) {
    editor.update_in(cx, |e, window, cx| {
        e.open_find_bar(show_replace, window, cx);
        e.find_bar.as_mut().unwrap().visible = false;
    });
}

/// Push `text` through the find input and its `Change` subscription (the
/// path a keystroke takes; `set_value` alone suppresses the event), then
/// scan. The debounced rescan is gated on a visible bar, so the scan is
/// invoked directly here.
fn set_find_query(editor: &Entity<SqlEditor>, cx: &mut gpui::VisualTestContext, text: &str) {
    let state = editor.read_with(cx, |e, _| e.find_bar.as_ref().unwrap().find.clone());
    set_input_value(&state, cx, text);
    editor.update(cx, |e, cx| e.rescan_find_matches(cx));
}

/// Set an input's text and deliver the `Change` a real keystroke would.
fn set_input_value(state: &Entity<InputState>, cx: &mut gpui::VisualTestContext, text: &str) {
    let text = text.to_string();
    cx.update(|window, cx| {
        state.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            cx.emit(InputEvent::Change);
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
fn find_bar_opens_seeded_from_selection_and_counts_matches(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| {
        e.set_text("select a; select b; select c;", cx);
        e.selection_anchor = Some(0);
        e.cursor = 6;
    });
    editor.update_in(cx, |e, window, cx| {
        e.open_find_bar(false, window, cx);
        {
            let bar = e.find_bar.as_ref().expect("bar created on open");
            assert!(bar.visible);
            assert!(!bar.show_replace);
        }
        e.find_bar.as_mut().unwrap().visible = false;
    });
    editor.read_with(cx, |e, cx| {
        let bar = e.find_bar.as_ref().unwrap();
        assert_eq!(bar.query, "select");
        assert_eq!(bar.find.read(cx).value().as_ref(), "select");
        assert_eq!(bar.matches.len(), 3);
        assert!(!bar.truncated);
        // The caret sits at byte 6, so the current match is the next one.
        assert_eq!(bar.current, Some(1));
    });
}

#[gpui::test]
fn find_bar_regex_toggle_reports_invalid_patterns(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("select 1", cx));
    open_hidden_find_bar(&editor, cx, false);
    editor.update(cx, |e, cx| e.toggle_find_regex(cx));
    set_find_query(&editor, cx, "(");
    editor.read_with(cx, |e, _| {
        let bar = e.find_bar.as_ref().unwrap();
        assert!(bar.regex);
        assert!(bar.compiled_query.is_none());
        assert!(
            bar.compile_error
                .as_ref()
                .is_some_and(|error| !error.is_empty())
        );
        assert!(bar.matches.is_empty());
    });
}

#[gpui::test]
fn regex_replace_all_expands_captures_and_is_one_undo(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("item-12 other-3", cx));
    open_hidden_find_bar(&editor, cx, true);
    editor.update(cx, |e, cx| e.toggle_find_regex(cx));
    set_find_query(&editor, cx, r"([a-z]+)-(\d+)");
    let replace_state = editor.read_with(cx, |e, _| e.find_bar.as_ref().unwrap().replace.clone());
    set_input_value(&replace_state, cx, "$2:$1");

    editor.update(cx, |e, cx| e.replace_all_matches(cx));
    editor.read_with(cx, |e, _| assert_eq!(e.text(), "12:item 3:other"));
    editor.update(cx, |e, cx| e.undo(cx));
    editor.read_with(cx, |e, _| assert_eq!(e.text(), "item-12 other-3"));
}

#[gpui::test]
fn zero_width_regex_replace_all_keeps_unicode_boundaries(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("α\nβ", cx));
    open_hidden_find_bar(&editor, cx, true);
    editor.update(cx, |e, cx| e.toggle_find_regex(cx));
    set_find_query(&editor, cx, "^");
    let replace_state = editor.read_with(cx, |e, _| e.find_bar.as_ref().unwrap().replace.clone());
    set_input_value(&replace_state, cx, "> ");

    editor.update(cx, |e, cx| e.replace_all_matches(cx));
    editor.read_with(cx, |e, _| assert_eq!(e.text(), "> α\n> β"));
}

#[gpui::test]
fn find_navigation_into_a_collapsed_fold_reveals_it(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| {
        e.set_text("BEGIN\n  SELECT needle;\nEND\nSELECT 1;", cx);
        e.toggle_fold_at_line(0, cx);
        assert_eq!(e.display_line_count(), 2);
    });
    open_hidden_find_bar(&editor, cx, false);
    set_find_query(&editor, cx, "needle");
    editor.update(cx, |e, cx| e.find_next(cx));
    cx.run_until_parked();
    // The match lives inside the hidden interior. The prepaint invariant
    // fires on the draw the update triggers, so by now the fold has
    // reopened on its own — the caret is visible and nothing is left for
    // reveal_cursor_folds to do.
    editor.update(cx, |e, _| {
        assert_eq!(
            e.display_line_count(),
            4,
            "prepaint must reopen the fold hiding the find match"
        );
        assert!(!e.reveal_cursor_folds(), "nothing should remain hidden");
    });
}

#[gpui::test]
fn replace_all_that_destroys_a_folded_region_reconciles_folds(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| {
        e.set_text("BEGIN\n  SELECT 1;\nEND", cx);
        e.toggle_fold_at_line(0, cx);
        assert_eq!(e.display_line_count(), 1);
    });
    open_hidden_find_bar(&editor, cx, true);
    set_find_query(&editor, cx, "END");
    editor.update(cx, |e, cx| e.replace_all_matches(cx));
    // Both BEGIN's END and the region are gone; the stale collapsed key
    // must be dropped rather than hiding live lines.
    editor.read_with(cx, |e, _| {
        assert_eq!(e.display_line_count(), 3, "no region may stay hidden");
    });
}

#[gpui::test]
fn find_next_and_prev_wrap_and_select_matches(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("a a a", cx));
    open_hidden_find_bar(&editor, cx, false);
    set_find_query(&editor, cx, "a");
    editor.read_with(cx, |e, _| {
        assert_eq!(e.find_bar.as_ref().unwrap().matches.len(), 3);
    });

    // The first step lands ON the match at the caret; later steps walk
    // forward and wrap.
    let expect = [(0, 1), (2, 3), (4, 5), (0, 1)];
    for (start, end) in expect {
        editor.update(cx, |e, cx| e.find_next(cx));
        editor.read_with(cx, |e, _| {
            assert_eq!(e.selection(), Some(start..end));
        });
    }
    editor.update(cx, |e, cx| e.find_prev(cx));
    editor.read_with(cx, |e, _| {
        assert_eq!(e.selection(), Some(4..5));
    });
}

#[gpui::test]
fn replace_current_selects_first_then_replaces_and_advances(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("foo bar foo", cx));
    open_hidden_find_bar(&editor, cx, true);
    set_find_query(&editor, cx, "foo");
    let replace_state = editor.read_with(cx, |e, _| e.find_bar.as_ref().unwrap().replace.clone());
    set_input_value(&replace_state, cx, "qux");

    // First invocation only highlights the target…
    editor.update(cx, |e, cx| e.replace_current(cx));
    editor.read_with(cx, |e, _| {
        assert_eq!(e.text(), "foo bar foo");
        assert_eq!(e.selection(), Some(0..3));
    });
    // …the second replaces it and advances to the next match.
    editor.update(cx, |e, cx| e.replace_current(cx));
    editor.read_with(cx, |e, _| {
        assert_eq!(e.text(), "qux bar foo");
        assert_eq!(e.selection(), Some(8..11));
    });
}

#[gpui::test]
fn replace_all_is_a_single_undo(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("a a a", cx));
    open_hidden_find_bar(&editor, cx, true);
    set_find_query(&editor, cx, "a");
    let replace_state = editor.read_with(cx, |e, _| e.find_bar.as_ref().unwrap().replace.clone());
    set_input_value(&replace_state, cx, "bb");

    editor.update(cx, |e, cx| e.replace_all_matches(cx));
    editor.read_with(cx, |e, _| assert_eq!(e.text(), "bb bb bb"));

    editor.update(cx, |e, cx| e.undo(cx));
    editor.read_with(cx, |e, _| assert_eq!(e.text(), "a a a"));
}

#[gpui::test]
fn dismiss_ladder_closes_the_bar_and_returns_focus(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| e.set_text("select 1", cx));
    open_hidden_find_bar(&editor, cx, false);
    editor.update(cx, |e, cx| {
        // Re-show inside this update (no draw happens mid-update) and run
        // the Esc ladder: a visible bar closes before the selection is
        // cleared, and closing hands focus back to the editor.
        e.find_bar.as_mut().unwrap().visible = true;
        e.selection_anchor = Some(0);
        e.needs_focus = false;
        e.dismiss_transient_ui(cx);
        let bar = e.find_bar.as_ref().unwrap();
        assert!(!bar.visible, "Esc closes the find bar first");
        assert_eq!(
            e.selection_anchor,
            Some(0),
            "the selection survives the Esc that closed the bar"
        );
        assert!(e.needs_focus, "closing returns focus to the editor");
    });
}

// ── Multi-cursor editing ───────────────────────────────────────────────

/// Extras of an editor as `(caret, anchor)` pairs, for compact asserts.
fn extras_of(e: &SqlEditor) -> Vec<(usize, Option<usize>)> {
    e.extra_cursors
        .iter()
        .map(|c| (c.cursor, c.anchor))
        .collect()
}

#[gpui::test]
fn extra_cursors_stay_sorted_deduped_and_off_the_primary(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("abcdefghij", cx);
        e.cursor = 5;
        e.add_extra_cursor(ExtraCursor::caret(7));
        e.add_extra_cursor(ExtraCursor::caret(2));
        e.add_extra_cursor(ExtraCursor::caret(7)); // duplicate
        e.add_extra_cursor(ExtraCursor::caret(5)); // on the primary
    });
    view.read_with(cx, |e, _| {
        assert_eq!(extras_of(e), vec![(2, None), (7, None)]);
    });
}

#[gpui::test]
fn extra_cursor_cap_is_enforced(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("x".repeat(200), cx);
        e.cursor = 0;
        for offset in 1..=150 {
            e.add_extra_cursor(ExtraCursor::caret(offset));
        }
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.extra_cursors.len(), MAX_EXTRA_CURSORS);
    });
}

#[gpui::test]
fn add_cursor_for_next_occurrence_selects_word_then_adds_and_stops(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo baz foo", cx);
        e.cursor = 1; // inside the first "foo", no selection
        e.add_cursor_for_next_occurrence(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.selection(), Some(0..3), "first press selects the word");
        assert!(e.extra_cursors.is_empty());
    });
    view.update(cx, |e, cx| e.add_cursor_for_next_occurrence(cx));
    view.read_with(cx, |e, _| {
        assert_eq!(extras_of(e), vec![(11, Some(8))]);
    });
    view.update(cx, |e, cx| e.add_cursor_for_next_occurrence(cx));
    view.read_with(cx, |e, _| {
        assert_eq!(extras_of(e), vec![(11, Some(8)), (19, Some(16))]);
    });
    // Every occurrence claimed: another press is a no-op.
    view.update(cx, |e, cx| e.add_cursor_for_next_occurrence(cx));
    view.read_with(cx, |e, _| {
        assert_eq!(extras_of(e), vec![(11, Some(8)), (19, Some(16))]);
    });
}

#[gpui::test]
fn add_cursor_for_next_occurrence_wraps_past_the_end(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo baz foo", cx);
        // Primary already on the LAST occurrence.
        e.selection_anchor = Some(16);
        e.cursor = 19;
        e.add_cursor_for_next_occurrence(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(
            extras_of(e),
            vec![(3, Some(0))],
            "search wraps to the first unclaimed occurrence"
        );
    });
}

#[gpui::test]
fn select_all_occurrences_covers_every_match(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo baz foo", cx);
        e.cursor = 9; // inside the second "foo"
        e.select_all_occurrences(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.selection(), Some(8..11), "primary keeps its occurrence");
        assert_eq!(extras_of(e), vec![(3, Some(0)), (19, Some(16))]);
    });
}

#[gpui::test]
fn occurrence_highlights_cover_every_other_copy_of_the_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo baz foo", cx);
        e.selection_anchor = Some(0);
        e.cursor = 3; // select the first "foo"
    });
    view.read_with(cx, |e, _| {
        let ranges = e.occurrence_highlights().expect("highlights for \"foo\"");
        assert_eq!(
            &*ranges,
            &[8..11, 16..19],
            "every copy except the selection"
        );
    });
}

#[gpui::test]
fn occurrence_highlights_skip_trivial_selections(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("ab  cd\nab", cx);
        // Single byte: too noisy to be useful.
        e.selection_anchor = Some(0);
        e.cursor = 1;
        assert!(e.occurrence_highlights().is_none(), "single-byte selection");
        // Whitespace only.
        e.selection_anchor = Some(2);
        e.cursor = 4;
        assert!(
            e.occurrence_highlights().is_none(),
            "whitespace-only selection"
        );
        // Spanning a newline.
        e.selection_anchor = Some(4);
        e.cursor = 8;
        assert!(e.occurrence_highlights().is_none(), "multi-line selection");
        // A real word: "ab" at 0..2 and 7..9; selecting the second finds the first.
        e.selection_anchor = Some(7);
        e.cursor = 9;
        let ranges = e.occurrence_highlights().expect("highlights for \"ab\"");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 0..2);
        // Longer than a short phrase: a block, not a name.
        let long = "x".repeat(600);
        e.set_text(format!("{long} {long}"), cx);
        e.selection_anchor = Some(0);
        e.cursor = 600;
        assert!(e.occurrence_highlights().is_none(), "over-long selection");
    });
}

#[gpui::test]
fn occurrence_highlights_skip_copies_overlapping_the_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("aaaa aa", cx);
        // Select the middle "aa" of "aaaa": the copies at 0..2 and 2..4 both
        // overlap it and must not paint under the selection.
        e.selection_anchor = Some(1);
        e.cursor = 3;
        let ranges = e.occurrence_highlights().expect("highlights");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 5..7);
    });
}

#[gpui::test]
fn occurrence_highlights_respect_the_preference_and_multi_cursor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo", cx);
        e.selection_anchor = Some(0);
        e.cursor = 3;
        // Preference off: nothing.
        e.editor_prefs.highlight_occurrences = false;
        assert!(e.occurrence_highlights().is_none(), "preference off");
        e.editor_prefs.highlight_occurrences = true;
        assert!(e.occurrence_highlights().is_some(), "preference back on");
        // Multi-cursor live: the highlight would fight the extra selections.
        e.add_cursor_for_next_occurrence(cx);
        assert!(e.occurrence_highlights().is_none(), "multi-cursor active");
    });
}

#[gpui::test]
fn occurrence_highlights_cache_is_selection_and_revision_keyed(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo", cx);
        e.selection_anchor = Some(0);
        e.cursor = 3;
        let first = e.occurrence_highlights().expect("highlights");
        let second = e.occurrence_highlights().expect("highlights");
        assert!(
            std::rc::Rc::ptr_eq(&first, &second),
            "same selection + revision reuses the cached ranges"
        );
        // An edit invalidates the cache even when the selection is unchanged.
        e.set_text("foo bar foo foo", cx);
        let third = e.occurrence_highlights().expect("highlights after edit");
        assert!(!std::rc::Rc::ptr_eq(&first, &third));
        assert_eq!(&*third, &[8..11, 12..15]);
    });
}

#[gpui::test]
fn add_cursor_above_below_follow_goal_column(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    // Line offsets: "abcdef" 0..6, "ab" 7..9, "abcdef" 10..16.
    view.update(cx, |e, cx| {
        e.set_text("abcdef\nab\nabcdef", cx);
        e.cursor = 4; // line 0, col 4
        e.add_cursor_vertical(false, cx); // below
    });
    view.read_with(cx, |e, _| {
        assert_eq!(
            extras_of(e),
            vec![(9, None)],
            "the short line clamps to its end"
        );
        assert_eq!(e.goal_column, Some(4), "the goal column sticks");
    });
    // The next press extends from the last-added caret, restoring col 4.
    view.update(cx, |e, cx| e.add_cursor_vertical(false, cx));
    view.read_with(cx, |e, _| {
        assert_eq!(extras_of(e), vec![(9, None), (14, None)]);
    });
    // And Above from a lower line clamps through the short line too.
    view.update(cx, |e, cx| {
        e.set_text("abcdef\nab\nabcdef", cx);
        e.cursor = 14; // line 2, col 4
        e.add_cursor_vertical(true, cx); // above
    });
    view.read_with(cx, |e, _| {
        assert_eq!(extras_of(e), vec![(9, None)]);
    });
}

#[gpui::test]
fn typing_inserts_at_every_cursor(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("aaa\nbbb\nccc", cx);
        e.cursor = 0;
        e.add_extra_cursor(ExtraCursor::caret(4));
        e.add_extra_cursor(ExtraCursor::caret(8));
    });
    view.update_in(cx, |e, window, cx| {
        e.replace_text_in_range(None, "-- ", window, cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "-- aaa\n-- bbb\n-- ccc");
        assert_eq!(e.cursor, 3, "primary advances past its insertion");
        assert_eq!(extras_of(e), vec![(10, None), (17, None)]);
    });
}

#[gpui::test]
fn backspace_at_adjacent_cursors_merges_instead_of_double_deleting(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("ab", cx);
        e.cursor = 2;
        e.add_extra_cursor(ExtraCursor::caret(1));
        e.delete_backward(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "", "each caret deletes exactly one char");
        assert_eq!(e.cursor, 0);
        assert!(
            e.extra_cursors.is_empty(),
            "carets collapsing onto the same offset merge"
        );
    });
}

#[gpui::test]
fn paste_distributes_lines_when_count_matches_cursors(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("a\nb\nc", cx);
        e.cursor = 0;
        e.add_extra_cursor(ExtraCursor::caret(2));
        e.add_extra_cursor(ExtraCursor::caret(4));
    });
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("X\nY\nZ".to_string()));
    view.update_in(cx, |e, window, cx| e.handle_paste(&Paste, window, cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "Xa\nYb\nZc");
        assert_eq!(e.cursor, 1);
        assert_eq!(extras_of(e), vec![(4, None), (7, None)]);
    });
}

#[gpui::test]
fn paste_inserts_whole_text_when_line_count_differs(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("a\nb\nc", cx);
        e.cursor = 0;
        e.add_extra_cursor(ExtraCursor::caret(2));
        e.add_extra_cursor(ExtraCursor::caret(4));
    });
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("X\nY".to_string()));
    view.update_in(cx, |e, window, cx| e.handle_paste(&Paste, window, cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "X\nYa\nX\nYb\nX\nYc");
    });
}

#[gpui::test]
fn multi_cursor_edit_is_one_undo_step_restoring_all_cursors(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("aaa\nbbb\nccc", cx);
        e.cursor = 0;
        e.add_extra_cursor(ExtraCursor::caret(4));
        e.add_extra_cursor(ExtraCursor::caret(8));
    });
    view.update_in(cx, |e, window, cx| {
        e.replace_text_in_range(None, "-- ", window, cx);
    });
    view.update(cx, |e, cx| e.undo(cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "aaa\nbbb\nccc", "one undo reverts the whole edit");
        assert_eq!(e.cursor, 0);
        assert_eq!(
            extras_of(e),
            vec![(4, None), (8, None)],
            "undo restores every caret"
        );
    });
}

#[gpui::test]
fn escape_clears_extra_cursors_before_the_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("foo bar foo", cx);
        e.selection_anchor = Some(0);
        e.cursor = 3;
        e.add_extra_cursor(ExtraCursor::caret(8));
        e.dismiss_transient_ui(cx);
        assert!(e.extra_cursors.is_empty(), "Esc clears the extras first");
        assert_eq!(
            e.selection_anchor,
            Some(0),
            "the primary selection survives that Esc"
        );
        e.dismiss_transient_ui(cx);
        assert_eq!(
            e.selection_anchor, None,
            "the next Esc clears the selection"
        );
    });
}

#[gpui::test]
fn plain_movement_clears_extra_cursors(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("aaa\nbbb", cx);
        e.cursor = 1;
        e.add_extra_cursor(ExtraCursor::caret(5));
        e.move_left(false, cx);
        assert!(e.extra_cursors.is_empty(), "arrow movement is primary-only");
        e.add_extra_cursor(ExtraCursor::caret(5));
        e.move_down(false, cx);
        assert!(e.extra_cursors.is_empty(), "vertical movement clears too");
    });
}

#[gpui::test]
fn find_bar_open_clears_extra_cursors(cx: &mut gpui::TestAppContext) {
    let (editor, cx) = find_bar_editor(cx);
    editor.update(cx, |e, cx| {
        e.set_text("foo bar foo", cx);
        e.cursor = 0;
        e.add_extra_cursor(ExtraCursor::caret(8));
    });
    open_hidden_find_bar(&editor, cx, false);
    editor.read_with(cx, |e, _| {
        assert!(e.extra_cursors.is_empty(), "find/replace is primary-only");
    });
}

#[gpui::test]
fn completion_accept_applies_to_each_cursor_as_one_edit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SEL SEL", cx);
        e.cursor = 3;
        e.add_extra_cursor(ExtraCursor::caret(7));
        e.completions = std::rc::Rc::new(make_test_completions(1));
        e.completion_index = 0;
        e.completion_visible = true;
        e.completion_replace_start = 0;
        e.accept_completion(cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "item_0 item_0");
        assert_eq!(e.cursor, 6);
        assert_eq!(e.extra_cursors, vec![ExtraCursor::caret(13)]);
    });
    view.update(cx, |e, cx| e.undo(cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SEL SEL");
        assert_eq!(e.cursor, 3);
        assert_eq!(e.extra_cursors, vec![ExtraCursor::caret(7)]);
    });
}

#[gpui::test]
fn fim_accept_applies_to_each_cursor_and_preserves_them(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("a b", cx);
        e.cursor = 1;
        e.add_extra_cursor(ExtraCursor::caret(3));
        e.show_fim_suggestion("!", cx);
        assert!(e.accept_fim_suggestion(cx));
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "a! b!");
        assert_eq!(e.cursor, 2);
        assert_eq!(e.extra_cursors, vec![ExtraCursor::caret(5)]);
    });
}

#[gpui::test]
fn rectangular_selection_edits_visible_columns_as_one_undo_step(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("abcd\nwxyz\n12", cx);
        editor.rectangular_anchor = Some((0, 1));
        let target = editor.offset_for_display_col(2, 3);
        editor.update_rectangular_selection(target, cx);
        editor.rectangular_anchor = None;
        editor.insert_at_all_cursors("X", cx);
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "aXcd\nwXyz\n1X");
        assert_eq!(editor.extra_cursors.len(), 2);
    });
    view.update(cx, |editor, cx| editor.undo(cx));
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "abcd\nwxyz\n12");
        assert_eq!(editor.selection(), Some(11..12));
        assert_eq!(editor.extra_cursors.len(), 2);
    });
}

#[gpui::test]
fn rectangular_columns_follow_tab_stops(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("a\tb\n12345", cx);
        assert_eq!(editor.display_col_for_offset(2), 4);
        assert_eq!(editor.offset_for_display_col(0, 4), 2);
        assert_eq!(editor.offset_for_display_col(1, 4), 8);
    });
}

#[gpui::test]
fn alt_drag_without_shift_starts_rectangular_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update_in(cx, |editor, window, cx| {
        editor.ddl_nav_prefs.enabled = false;
        editor.set_text("abc\ndef\nghi\njkl", cx);
        editor.last_bounds = Some(gpui::Bounds {
            origin: gpui::Point::new(px(0.0), px(0.0)),
            size: gpui::size(px(400.0), px(400.0)),
        });
        let line_h = f32::from(editor.line_height);
        let x = f32::from(editor.line_number_width) + 24.0;
        editor.handle_mouse_down(
            &gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position: gpui::Point::new(px(x), px(line_h * 0.5)),
                modifiers: gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
                click_count: 1,
                first_mouse: false,
            },
            window,
            cx,
        );
        editor.handle_mouse_move(
            &gpui::MouseMoveEvent {
                position: gpui::Point::new(px(x), px(line_h * 2.5)),
                modifiers: gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
                pressed_button: Some(gpui::MouseButton::Left),
            },
            window,
            cx,
        );
        assert_eq!(editor.extra_cursors.len(), 2);
        assert!(editor.extra_cursors.iter().all(|c| c.anchor.is_some()));
    });
}

#[gpui::test]
fn column_select_down_extends_one_line_per_press(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("abc\ndef\nghi\njkl", cx);
        editor.cursor = 2;
        editor.handle_column_select(true, cx);
        editor.handle_column_select(true, cx);
        assert_eq!(editor.extra_cursors.len(), 2);
        let (line, col) = editor.line_col_for_offset(editor.cursor);
        assert_eq!(line, 2);
        assert_eq!(col, 2);
    });
}

#[gpui::test]
fn column_select_typing_inserts_on_every_line(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("abc\ndef\nghi\njkl", cx);
        editor.cursor = 2;
        editor.handle_column_select(true, cx);
        editor.handle_column_select(true, cx);
        editor.insert_at_all_cursors("x", cx);
        assert_eq!(editor.text(), "abxc\ndexf\nghxi\njkl");
    });
}

// ── Same-buffer split: one document, two views ─────────────────────────

/// A canonical editor (window root) plus a second view sharing its
/// document, as the same-buffer split wires them.
fn shared_pair(
    cx: &mut gpui::TestAppContext,
) -> (
    gpui::Entity<SqlEditor>,
    gpui::Entity<SqlEditor>,
    &mut gpui::VisualTestContext,
) {
    let (canonical, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let view = cx.update(|_window, app| SqlEditor::new_shared_view(&canonical, app));
    (canonical, view, cx)
}

#[gpui::test]
fn same_buffer_views_share_edits_in_both_directions(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("SELECT 1", cx));
    cx.run_until_parked();
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 1"));

    // Typed via the canonical entity, read via the view.
    canonical.update(cx, |e, cx| {
        e.cursor = e.text().len();
        e.insert(" -- a", cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 1 -- a"));

    // Typed via the view entity, read via the canonical.
    view.update(cx, |e, cx| {
        e.cursor = e.text().len();
        e.insert(" -- b", cx);
    });
    cx.run_until_parked();
    canonical.read_with(cx, |e, _| assert_eq!(e.text(), "SELECT 1 -- a -- b"));
}

#[gpui::test]
fn same_buffer_undo_is_one_stream_with_sane_cursors(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("base", cx));
    cx.run_until_parked();

    canonical.update(cx, |e, cx| {
        e.cursor = 4;
        e.insert(" one", cx);
    });
    cx.run_until_parked();
    view.update(cx, |e, cx| {
        e.cursor = e.text().len();
        e.insert(" two", cx);
    });
    cx.run_until_parked();
    canonical.read_with(cx, |e, _| {
        assert_eq!(e.text(), "base one two");
        assert_eq!(e.undo_stack.len(), 2, "one undo stream for the document");
    });

    // Undo from the canonical view rewinds the view's edit — the shared
    // document steps back exactly once.
    canonical.update(cx, |e, cx| e.undo(cx));
    cx.run_until_parked();
    canonical.read_with(cx, |e, _| {
        assert_eq!(e.text(), "base one");
        assert!(e.cursor <= e.text().len(), "cursor stays in bounds");
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "base one");
        assert!(e.cursor <= e.text().len(), "peer cursor clamps sanely");
    });

    // Undo from the second view rewinds the canonical's edit.
    view.update(cx, |e, cx| e.undo(cx));
    cx.run_until_parked();
    canonical.read_with(cx, |e, _| assert_eq!(e.text(), "base"));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "base");
        assert!(e.cursor <= e.text().len());
    });

    // And redo replays it from either side.
    canonical.update(cx, |e, cx| e.redo(cx));
    cx.run_until_parked();
    view.read_with(cx, |e, _| assert_eq!(e.text(), "base one"));
}

#[gpui::test]
fn same_buffer_cursor_and_extra_cursors_stay_per_view(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("aaa\nbbb\nccc", cx));
    cx.run_until_parked();

    canonical.update(cx, |e, _| e.cursor = 1);
    view.update(cx, |e, _| e.cursor = 9);
    canonical.update(cx, |e, _| e.add_extra_cursor(ExtraCursor::caret(5)));
    cx.run_until_parked();

    canonical.read_with(cx, |e, _| {
        assert_eq!(e.cursor, 1);
        assert_eq!(e.extra_cursors.len(), 1);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.cursor, 9, "the view keeps its own caret");
        assert!(e.extra_cursors.is_empty(), "extra cursors are per-view");
    });
}

#[gpui::test]
fn same_buffer_adoption_clamps_carets_to_the_new_text(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("0123456789😊", cx));
    cx.run_until_parked();
    view.update(cx, |e, _| {
        e.cursor = "0123456789😊".len();
        e.add_extra_cursor(ExtraCursor::caret(11)); // mid-emoji offset
    });

    // The canonical shrinks the document; the view's carets must snap
    // into the new text on valid char boundaries.
    canonical.update(cx, |e, cx| e.set_text("0123", cx));
    cx.run_until_parked();
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "0123");
        assert!(e.cursor <= 4);
        assert!(e.text().is_char_boundary(e.cursor));
        for ec in &e.extra_cursors {
            assert!(ec.cursor <= 4 && e.text().is_char_boundary(ec.cursor));
        }
    });
}

#[gpui::test]
fn same_buffer_views_share_diagnostics(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| {
        e.set_text("DELETE FROM t", cx);
        e.set_diagnostics(
            vec![ValidationDiagnostic {
                severity: DiagnosticSeverity::Warning,
                source: DiagnosticSource::Validation,
                message: "no WHERE".into(),
                line: 0,
                identifier: None,
                start_byte: 0,
                end_byte: 6,
                explainer: None,
                fix: None,
            }],
            cx,
        );
    });
    cx.run_until_parked();
    view.read_with(cx, |e, _| {
        assert_eq!(e.diagnostics.len(), 1, "diagnostics are document state");
        assert_eq!(e.diagnostics[0].message, "no WHERE");
    });
    canonical.update(cx, |e, cx| e.clear_diagnostics(cx));
    cx.run_until_parked();
    view.read_with(cx, |e, _| assert!(e.diagnostics.is_empty()));
}

#[gpui::test]
fn execution_error_is_dropped_on_edit_while_validation_survives(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT Nmae FROM T", cx);
        editor.set_diagnostics(
            vec![ValidationDiagnostic {
                severity: DiagnosticSeverity::Warning,
                source: DiagnosticSource::Validation,
                message: "lint".into(),
                line: 0,
                identifier: None,
                start_byte: 0,
                end_byte: 6,
                explainer: None,
                fix: None,
            }],
            cx,
        );
        editor.set_execution_error(
            ValidationDiagnostic {
                severity: DiagnosticSeverity::Error,
                source: DiagnosticSource::Execution,
                message: "Invalid column".into(),
                line: 0,
                identifier: None,
                start_byte: 7,
                end_byte: 11,
                explainer: None,
                fix: None,
            },
            cx,
        );
        assert!(
            editor
                .validation_diagnostics()
                .iter()
                .any(|d| d.source == DiagnosticSource::Execution)
        );
        editor.insert("x", cx);
        let remaining: Vec<_> = editor
            .validation_diagnostics()
            .iter()
            .map(|d| d.source)
            .collect();
        assert_eq!(remaining, [DiagnosticSource::Validation]);
    });
}

#[gpui::test]
fn same_buffer_views_share_buffer_derived_caches(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    let (line_index, shapes, cols, counter) = canonical.read_with(cx, |c, _| {
        (
            c.line_index_cache.clone(),
            c.shape_cache.clone(),
            c.max_display_cols_cache.clone(),
            c.revision_counter.clone(),
        )
    });
    view.read_with(cx, |v, _| {
        assert!(
            std::rc::Rc::ptr_eq(&line_index, &v.line_index_cache),
            "line index is built once per document, not per view"
        );
        assert!(
            std::rc::Rc::ptr_eq(&shapes, &v.shape_cache),
            "shaped lines are cached once per document"
        );
        assert!(std::rc::Rc::ptr_eq(&cols, &v.max_display_cols_cache));
        assert!(
            std::rc::Rc::ptr_eq(&counter, &v.revision_counter),
            "revisions come from one counter so cache keys can't collide"
        );
    });
}

/// Counts `Change` events from an editor entity, standing in for the app
/// layer's per-tab dirty/validation pipeline (which subscribes to BOTH
/// views of a same-buffer split and maps either to the tab).
struct ChangeProbe {
    changes: usize,
}

#[gpui::test]
fn view_edits_emit_change_for_the_app_and_adoption_stays_silent(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("x", cx));
    cx.run_until_parked();
    let probe = cx.update(|_window, app| {
        let canonical = canonical.clone();
        let view = view.clone();
        app.new(|cx: &mut gpui::Context<ChangeProbe>| {
            let count = |probe: &mut ChangeProbe,
                         _,
                         event: &SqlEditorEvent,
                         _: &mut gpui::Context<ChangeProbe>| {
                if matches!(event, SqlEditorEvent::Change) {
                    probe.changes += 1;
                }
            };
            cx.subscribe(&canonical, count).detach();
            cx.subscribe(&view, count).detach();
            ChangeProbe { changes: 0 }
        })
    });

    // An edit typed into the second view emits Change from that view —
    // the app's per-tab pipeline (dirty flag, validation, history) is
    // subscribed there too, which is what keeps the dirty flag shared.
    view.update(cx, |e, cx| {
        e.cursor = 1;
        e.insert("y", cx);
    });
    cx.run_until_parked();
    assert!(
        probe.read_with(cx, |p, _| p.changes) >= 1,
        "edits in the view surface as Change for the app"
    );
    canonical.read_with(cx, |e, _| assert_eq!(e.text(), "xy"));

    // A programmatic load (set_text emits no Change) must stay silent
    // through adoption as well — otherwise opening a file with a split
    // up would falsely dirty the tab.
    let before = probe.read_with(cx, |p, _| p.changes);
    canonical.update(cx, |e, cx| e.set_text("loaded", cx));
    cx.run_until_parked();
    view.read_with(cx, |e, _| assert_eq!(e.text(), "loaded"));
    assert_eq!(
        probe.read_with(cx, |p, _| p.changes),
        before,
        "adopting a silent programmatic load emits no Change"
    );
}

#[gpui::test]
fn closing_the_split_carries_the_focused_views_caret(cx: &mut gpui::TestAppContext) {
    // The app's close_split: when the second view was focused, its caret
    // is adopted by the surviving editor before the view is dropped.
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("SELECT 1 FROM t", cx));
    cx.run_until_parked();
    view.update(cx, |e, _| {
        e.cursor = 9;
        e.selection_anchor = Some(14);
    });
    let (cursor, anchor) = view.read_with(cx, |e, _| e.caret_state());
    canonical.update(cx, |e, cx| {
        e.adopt_caret_state(cursor, anchor, cx);
        e.detach_shared_peer();
    });
    canonical.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT 1 FROM t", "content survives the close");
        assert_eq!(e.cursor, 9);
        assert_eq!(e.selection_anchor, Some(14));
    });
    // Out-of-range carets clamp instead of panicking.
    canonical.update(cx, |e, cx| e.adopt_caret_state(999, Some(999), cx));
    canonical.read_with(cx, |e, _| {
        assert_eq!(e.cursor, e.text().len());
        assert_eq!(e.selection_anchor, None, "anchor == cursor collapses");
    });
}

#[gpui::test]
fn detaching_the_shared_peer_stops_the_mirror(cx: &mut gpui::TestAppContext) {
    let (canonical, view, cx) = shared_pair(cx);
    canonical.update(cx, |e, cx| e.set_text("kept", cx));
    cx.run_until_parked();

    // Closing the split: both sides unlink; the canonical keeps the text.
    canonical.update(cx, |e, _| e.detach_shared_peer());
    view.update(cx, |e, _| e.detach_shared_peer());

    canonical.update(cx, |e, cx| {
        e.cursor = 4;
        e.insert(" going", cx);
    });
    cx.run_until_parked();
    canonical.read_with(cx, |e, _| assert_eq!(e.text(), "kept going"));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "kept", "a detached view no longer mirrors");
    });
}

/// Renders the two views of one document side by side, standing in for
/// the app's same-buffer split layout.
struct SplitHost {
    first: gpui::Entity<SqlEditor>,
    second: gpui::Entity<SqlEditor>,
}

impl Render for SplitHost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .child(div().flex_1().min_w_0().h_full().child(self.first.clone()))
            .child(div().flex_1().min_w_0().h_full().child(self.second.clone()))
    }
}

#[gpui::test]
fn same_buffer_split_renders_both_panes(cx: &mut gpui::TestAppContext) {
    // Install the component theme first, per the windowed-test ground
    // rules. (The window root stays a plain view: gpui-component's
    // `Root::new` panics on gpui's TestWindow on macOS — see the ignored
    // Root test in results.rs — and the editor needs no Root services.)
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let first = cx.new(|cx| {
            let mut editor = SqlEditor::new(cx);
            editor.set_text("SELECT 42", cx);
            editor
        });
        let second = SqlEditor::new_shared_view(&first, cx);
        SplitHost { first, second }
    });
    cx.run_until_parked();

    // Both views painted: each stamped its bounds and shaped the shared
    // document's lines during the frame.
    host.read_with(cx, |host, cx| {
        let first = host.first.read(cx);
        let second = host.second.read(cx);
        assert!(first.last_bounds.is_some(), "leading pane painted");
        assert!(second.last_bounds.is_some(), "trailing pane painted");
        assert!(!first.last_layout.is_empty());
        assert!(!second.last_layout.is_empty());
        assert_eq!(first.text(), second.text());
    });
}

// ── Snippet tab-stop sessions ──────────────────────────────────────────

#[gpui::test]
fn snippet_expand_selects_first_stop(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.expand_snippet(0..0, "SELECT ${1:100} FROM ${2:t}$0", cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "SELECT 100 FROM t");
        assert_eq!(e.selection(), Some(7..10));
    });
}

#[gpui::test]
fn snippet_tab_cycles_and_shift_tab_returns(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.expand_snippet(0..0, "SELECT ${1:100} FROM ${2:t}$0", cx);
        e.next_snippet_stop(cx);
        assert_eq!(e.selection(), Some(16..17));
        e.previous_snippet_stop(cx);
        assert_eq!(e.selection(), Some(7..10));
    });
}

#[gpui::test]
fn snippet_typing_replaces_stop_and_shifts_later_stops(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.expand_snippet(0..0, "SELECT ${1:100} FROM ${2:t}$0", cx);
        e.replace_selection("5", cx);
        e.next_snippet_stop(cx);
        assert_eq!(e.text(), "SELECT 5 FROM t");
        assert_eq!(e.selection(), Some(14..15));
    });
}

#[gpui::test]
fn snippet_mirrored_stop_types_at_every_range(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.expand_snippet(0..0, "${1:x} + ${1:x}", cx);
        e.insert_at_all_cursors("y", cx);
        assert_eq!(e.text(), "y + y");
    });
}

#[gpui::test]
fn snippet_final_stop_ends_session(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.expand_snippet(0..0, "SELECT ${1:100} FROM ${2:t}$0", cx);
        e.next_snippet_stop(cx);
        e.next_snippet_stop(cx);
        assert!(e.snippet_session.is_none());
        assert_eq!(e.cursor, e.content.len());
        assert_eq!(e.selection_anchor, None);
    });
}

#[gpui::test]
fn snippet_prefix_tab_expands_user_prefix(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_completion_catalog(
            Arc::new(CompletionCatalog {
                snippets: vec![("sel".into(), "SELECT $1".into())],
                ..Default::default()
            }),
            cx,
        );
        e.set_text("sel", cx);
        e.cursor = e.content.len();
        assert!(e.try_expand_snippet_prefix(cx));
        assert_eq!(e.text(), "SELECT ");
        assert_eq!(e.cursor, 7);
    });
}

#[gpui::test]
fn snippet_tab_without_session_still_indents(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("", cx);
        e.insert_tab(cx);
        assert_eq!(e.text(), e.editor_prefs.indent_unit());
        assert!(e.snippet_session.is_none());
    });
}

#[gpui::test]
fn snippet_escape_ends_session_before_clearing_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.expand_snippet(0..0, "SELECT ${1:100} FROM ${2:t}$0", cx);
        assert!(e.snippet_session.is_some());
        let selection = e.selection();
        e.dismiss_transient_ui(cx);
        assert!(e.snippet_session.is_none());
        assert_eq!(
            e.selection(),
            selection,
            "first Esc keeps the stop selected"
        );
    });
}

#[gpui::test]
fn snippet_edit_outside_span_ends_session(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("zz", cx);
        e.expand_snippet(2..2, "SELECT ${1:100}$0", cx);
        assert!(e.snippet_session.is_some());
        e.cursor = 0;
        e.selection_anchor = None;
        e.replace_selection("!", cx);
        assert!(e.snippet_session.is_none());
    });
}

#[gpui::test]
fn snippet_expand_with_extra_cursors_uses_primary_only(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("hello", cx);
        e.cursor = 5;
        e.add_extra_cursor(ExtraCursor::caret(0));
        e.expand_snippet(5..5, "${1:x}", cx);
        assert_eq!(e.text(), "hellox");
        assert!(e.extra_cursors.is_empty());
        assert_eq!(e.selection(), Some(5..6));
    });
}

fn enable_vim(editor: &mut SqlEditor, cx: &mut gpui::Context<SqlEditor>) {
    editor.set_editor_prefs(
        EditorPreferences {
            vim_mode: true,
            ..EditorPreferences::default()
        },
        cx,
    );
}

#[gpui::test]
fn vim_off_by_default_types_normally(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.read_with(cx, |e, _| assert_eq!(e.vim_mode_label(), None));
    cx.simulate_input("x");
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "x");
        assert_eq!(e.vim_mode_label(), None);
    });
}

#[gpui::test]
fn vim_normal_mode_x_deletes_char(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("select", cx);
        e.cursor = 0;
    });
    cx.simulate_input("x");
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "elect");
        assert_eq!(e.vim_mode_label(), Some("NORMAL"));
    });
}

#[gpui::test]
fn vim_insert_then_escape_returns_to_normal(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("select", cx);
        e.cursor = 0;
    });
    cx.simulate_input("i");
    cx.simulate_input("ab");
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "abselect");
        assert_eq!(e.vim_mode_label(), Some("INSERT"));
    });
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |e, _| {
        assert_eq!(e.vim_mode_label(), Some("NORMAL"));
        assert_eq!(e.cursor(), 1);
        assert_eq!(e.text(), "abselect");
    });
}

#[gpui::test]
fn vim_dd_is_one_undo_step(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("a\nb\nc", cx);
        e.cursor = 2;
    });
    cx.simulate_input("dd");
    view.read_with(cx, |e, _| assert_eq!(e.text(), "a\nc"));
    cx.simulate_input("u");
    view.read_with(cx, |e, _| assert_eq!(e.text(), "a\nb\nc"));
    cx.simulate_keystrokes("ctrl-r");
    view.read_with(cx, |e, _| assert_eq!(e.text(), "a\nc"));
}

#[gpui::test]
fn vim_visual_delete_via_clipboard(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("ab cd", cx);
        e.cursor = 0;
    });
    cx.simulate_input("vlld");
    view.read_with(cx, |e, _| assert_eq!(e.text(), " cd"));
    let clip = cx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(clip.as_deref(), Some("ab"));
}

#[gpui::test]
fn vim_yy_p_pastes_linewise(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("ab", cx);
        e.cursor = 0;
    });
    cx.simulate_input("yyp");
    view.read_with(cx, |e, _| assert_eq!(e.text(), "ab\nab"));
}

#[gpui::test]
fn vim_slash_opens_find_bar(cx: &mut gpui::TestAppContext) {
    let (view, cx) = find_bar_editor(cx);
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("select 1", cx);
    });
    view.update_in(cx, |e, window, cx| {
        e.focus_handle.focus(window, cx);
        // Drive `/` in the same update that hides the overlay so
        // gpui-component `Input` never paints on a TestWindow (see
        // `open_hidden_find_bar`). `simulate_input("/")` would open the
        // bar and then draw, which panics.
        e.feed_vim(VimKey::Char('/'), Some(window), cx);
        assert!(e.find_bar.as_ref().is_some_and(|b| b.visible));
        e.find_bar.as_mut().unwrap().visible = false;
    });
}

#[gpui::test]
fn vim_enter_in_normal_moves_down_not_newline(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("a\nb", cx);
        e.cursor = 0;
    });
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "a\nb");
        assert_eq!(e.cursor(), 2);
    });
}

#[gpui::test]
fn vim_escape_ladder_still_closes_popups_in_normal(cx: &mut gpui::TestAppContext) {
    // Component theme first: the dialog chrome renders gpui-component Buttons.
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("SELECT CreatedAt", cx);
        e.open_result_column_format_dialog("CreatedAt".into(), "datetime2", None, cx);
    });
    cx.run_until_parked();
    assert!(view.read_with(cx, |e, _| e.format_dialog.is_some()));
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |e, _| {
        assert!(e.format_dialog.is_none());
        assert_eq!(e.vim_mode_label(), Some("NORMAL"));
    });
}

#[gpui::test]
fn vim_disabling_pref_restores_modeless_editing(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("", cx);
        e.toggle_vim_mode(cx);
    });
    cx.simulate_input("x");
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "x");
        assert_eq!(e.vim_mode_label(), None);
    });
}

#[gpui::test]
fn vim_ime_composition_bypasses_modal_layer(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        enable_vim(e, cx);
        e.set_text("", cx);
    });
    view.update_in(cx, |e, window, cx| {
        e.replace_and_mark_text_in_range(None, "か", None, window, cx);
    });
    view.update_in(cx, |e, window, cx| {
        e.replace_text_in_range(None, "か", window, cx);
    });
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), "か");
        assert!(e.marked_range.is_none());
        assert_eq!(e.vim_mode_label(), Some("NORMAL"));
    });
}

#[test]
fn gutter_width_includes_play_column() {
    assert_eq!(SqlEditor::gutter_width(true), px(54.0));
    assert_eq!(SqlEditor::gutter_width(false), px(0.0));
}

#[gpui::test]
fn gutter_play_click_emits_run_statement_at(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "SELECT 1;\nSELECT 2;";
    view.update(cx, |editor, cx| {
        editor.set_text(sql, cx);
        editor.editor_prefs.show_line_numbers = true;
        editor.line_number_width = SqlEditor::gutter_width(true);
        editor.last_bounds = Some(gpui::Bounds {
            origin: gpui::Point::new(px(0.0), px(0.0)),
            size: gpui::size(px(400.0), px(200.0)),
        });
    });
    let offset = view.read_with(cx, |editor, _| {
        let y = editor.line_height + px(2.0);
        editor.gutter_play_target(gpui::Point::new(px(5.0), y))
    });
    assert_eq!(offset, Some(10));
}

#[gpui::test]
fn frame_hidden_while_selection_exists(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT 1;\nSELECT 2;", cx);
        editor.cursor = 12;
        editor.selection_anchor = None;
        assert_eq!(editor.statement_span_at_cursor(), Some((10, 19)));
        editor.selection_anchor = Some(0);
        assert_eq!(editor.statement_span_at_cursor(), None);
    });
}

#[gpui::test]
fn result_source_clears_on_edit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT 1;", cx);
        editor.set_result_source(
            Some(ResultSource {
                span: (0, 9),
                outcome: RunOutcome::Succeeded,
            }),
            cx,
        );
        assert!(editor.result_source().is_some());
        editor.insert("x", cx);
        assert!(editor.result_source().is_none());
    });
}

#[gpui::test]
fn paste_as_in_list_from_clipboard(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_database_type(sqlly_lang::sqlmodel::DatabaseType::SqlServer, cx);
        editor.set_text("WHERE id IN ", cx);
        editor.cursor = editor.content.len();
    });
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("1\n2\nabc".to_string()));
    view.update_in(cx, |editor, window, cx| {
        editor.handle_paste_as_in_list(&PasteAsInList, window, cx);
    });
    view.read_with(cx, |editor, _| {
        assert_eq!(editor.text(), "WHERE id IN (1, 2, N'abc')");
    });
}

#[gpui::test]
fn convert_delimited_list_opens_dialog_for_selection(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("a,b,c", cx);
        editor.selection_anchor = Some(0);
        editor.cursor = editor.content.len();
    });
    view.update_in(cx, |editor, window, cx| {
        editor.handle_convert_delimited_list(&ConvertDelimitedList, window, cx);
    });
    view.read_with(cx, |editor, _| assert!(editor.convert_dialog.is_some()));
}

#[gpui::test]
fn convert_delimited_list_preview_quotes_selection(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update_in(cx, |editor, window, cx| {
        editor.set_text("a,b,c", cx);
        editor.selection_anchor = Some(0);
        editor.cursor = editor.content.len();
        editor.handle_convert_delimited_list(&ConvertDelimitedList, window, cx);
        if let Some(dialog) = editor.convert_dialog.as_mut() {
            dialog.quote = crate::list_tools::Quote::Single;
            let preview = dialog.preview(cx).unwrap();
            assert_eq!(preview, "'a', 'b', 'c'");
            editor.apply_convert_list_replace(cx);
        }
    });
    view.read_with(cx, |editor, _| assert_eq!(editor.text(), "'a', 'b', 'c'"));
}

#[test]
fn value_completions_use_the_connections_string_rules() {
    use sqlly_lang::sqlmodel::DatabaseType;
    use sqlly_services::ValueOperator::{Equals, Like};
    // SQL Server: plain quotes for plain text, N'…' only when needed.
    assert_eq!(
        value_completion_literal(DatabaseType::SqlServer, "O'Brien", Equals),
        "'O''Brien'"
    );
    assert_eq!(
        value_completion_literal(DatabaseType::SqlServer, "Zoë", Equals),
        "N'Zoë'"
    );
    assert_eq!(
        value_completion_literal(DatabaseType::SqlServer, "a\\", Equals),
        "'a\\'"
    );
    // MySQL: a trailing backslash can no longer escape the closing quote.
    assert_eq!(
        value_completion_literal(DatabaseType::MySql, "a\\", Equals),
        "'a\\\\'"
    );
    assert_eq!(
        value_completion_literal(DatabaseType::MySql, "x", Like),
        "'%x%'"
    );
    assert_eq!(
        value_completion_literal(DatabaseType::Postgres, "42", Equals),
        "42"
    );
}

#[gpui::test]
fn convert_delimited_list_requires_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update_in(cx, |editor, window, cx| {
        editor.handle_convert_delimited_list(&ConvertDelimitedList, window, cx);
        assert!(editor.convert_dialog.is_none());
    });
}

#[gpui::test]
fn esc_dismisses_hover_card_before_other_transient_ui(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.hover_info = Some((
            HoverInfo::Table {
                qualified_name: "dbo.Orders".into(),
                is_view: false,
                columns: vec![HoverColumn {
                    name: "Id".into(),
                    data_type: "int".into(),
                    nullable: Some(false),
                    is_primary_key: true,
                }],
                more_columns: 0,
            },
            (0, 5),
        ));
        e.completion_visible = true;
        e.dismiss_transient_ui(cx);
        assert!(e.hover_info.is_none());
        assert!(e.completion_visible, "completion stays until the next Esc");
    });
}

#[gpui::test]
fn signature_help_refreshes_on_cursor_move_and_respects_dismissal(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        let sql = "SELECT DATEADD(day, 1, x)";
        e.set_text(sql, cx);
        let open = sql.find('(').expect("paren");
        e.cursor = open + 1;
        e.refresh_signature_help(cx);
        assert!(e.signature_help.is_some(), "inside DATEADD(");
        e.dismiss_transient_ui(cx);
        assert!(e.signature_help.is_none());
        e.cursor = open + 6; // still inside the same parens
        e.refresh_signature_help(cx);
        assert!(
            e.signature_help.is_none(),
            "Esc dismissal sticks until the caret leaves the call"
        );
        e.cursor = 0;
        e.refresh_signature_help(cx);
        e.cursor = open + 1;
        e.refresh_signature_help(cx);
        assert!(
            e.signature_help.is_some(),
            "help returns after leaving and re-entering the call"
        );
    });
}

/// Signature help scans only the caret's statement; offsets it reports
/// (and Esc dismissal keyed on them) stay document-absolute.
#[gpui::test]
fn signature_help_in_a_later_statement_keeps_absolute_offsets(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        let sql = "SELECT 1;\nSELECT LEN(N'a');\nSELECT DATEADD(day, 1, x);";
        e.set_text(sql, cx);
        let open = sql.find("DATEADD(").expect("call") + "DATEADD".len();
        e.cursor = open + 1;
        e.refresh_signature_help(cx);
        let help = e.signature_help.clone().expect("inside DATEADD(");
        assert_eq!(help.open_paren, open);
        e.dismiss_transient_ui(cx);
        e.cursor = open + 6;
        e.refresh_signature_help(cx);
        assert!(
            e.signature_help.is_none(),
            "dismissal holds inside the call"
        );
        e.cursor = sql.find("LEN(").expect("len") + 4;
        e.refresh_signature_help(cx);
        assert!(
            e.signature_help.is_some(),
            "another call lifts the dismissal"
        );
    });
}

#[gpui::test]
fn hover_preference_off_never_schedules_resolve(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.editor_prefs.show_hover_info = false;
        e.schedule_hover_resolve(cx);
        assert!(e.hover_dwell_task.is_none());
    });
}

#[gpui::test]
fn routine_signatures_rebuilt_on_set_completion_procedures(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        let mut proc = ProcedureInfo::new("dbo", "GetOrder");
        proc.parameters.push(ProcedureParameter::new("@Id", "int"));
        e.set_completion_procedures(Arc::new(vec![proc]), cx);
        assert_eq!(e.routine_signatures.len(), 1);
        assert_eq!(e.routine_signatures[0].name, "GetOrder");
        assert_eq!(e.routine_signatures[0].parameters[0].data_type, "int");
    });
}

#[gpui::test]
fn insert_hints_follow_catalog_and_content(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "INSERT INTO orders VALUES (1, 'a')";
    view.update(cx, |editor, cx| {
        editor.set_completion_catalog(
            Arc::new(CompletionCatalog {
                objects: vec![CompletionObject::table(
                    "dbo",
                    "orders",
                    vec!["id".into(), "customer".into()],
                )],
                ..Default::default()
            }),
            cx,
        );
        editor.set_text(sql, cx);
        editor.recompute_insert_hints_now();
        let hints = editor.insert_hints_snapshot();
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].label, "id:");
        assert_eq!(hints[1].label, "customer:");
        assert_eq!(&sql[hints[0].offset..hints[0].offset + 1], "1");
        assert_eq!(&sql[hints[1].offset..hints[1].offset + 3], "'a'");
    });
}

#[gpui::test]
fn insert_hints_clear_when_preference_off(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_completion_catalog(
            Arc::new(CompletionCatalog {
                objects: vec![CompletionObject::table(
                    "dbo",
                    "orders",
                    vec!["id".into(), "customer".into()],
                )],
                ..Default::default()
            }),
            cx,
        );
        editor.set_text("INSERT INTO orders VALUES (1, 'a')", cx);
        editor.recompute_insert_hints_now();
        assert_eq!(editor.insert_hints_snapshot().len(), 2);
        let mut prefs = editor.editor_prefs.clone();
        prefs.show_insert_value_hints = false;
        editor.set_editor_prefs(prefs, cx);
        assert!(editor.insert_hints_snapshot().is_empty());
    });
}

#[gpui::test]
fn insert_hints_are_not_painted_for_stale_revision(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_completion_catalog(
            Arc::new(CompletionCatalog {
                objects: vec![CompletionObject::table(
                    "dbo",
                    "orders",
                    vec!["id".into(), "customer".into()],
                )],
                ..Default::default()
            }),
            cx,
        );
        editor.set_text("INSERT INTO orders VALUES (1, 'a')", cx);
        editor.recompute_insert_hints_now();
        assert_eq!(editor.insert_hints_snapshot().len(), 2);
        editor.cursor = editor.content.len();
        editor.replace_selection(" ", cx);
        assert!(
            editor.insert_hints_snapshot().is_empty(),
            "stale hints from the previous revision must not paint"
        );
    });
}

#[gpui::test]
fn insert_hints_refresh_synchronously_for_small_buffers(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_completion_catalog(
            Arc::new(CompletionCatalog {
                objects: vec![CompletionObject::table(
                    "dbo",
                    "orders",
                    vec!["id".into(), "customer".into()],
                )],
                ..Default::default()
            }),
            cx,
        );
        editor.set_text("INSERT INTO orders VALUES (1, 'a')", cx);
        editor.recompute_insert_hints_now();
        assert_eq!(editor.insert_hints_snapshot().len(), 2);
        // A keystroke bumps the revision; the layout pass then refreshes
        // hints synchronously so labels never blank for the debounce.
        editor.cursor = editor.content.len();
        editor.replace_selection(";", cx);
        assert!(editor.insert_hints_snapshot().is_empty());
        editor.ensure_insert_hints_current();
        let hints = editor.insert_hints_snapshot();
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].label, "id:");
    });
}

fn identity_snapshot() -> std::sync::Arc<sqlly_lang::sqlmodel::ModelSnapshot> {
    use sqlly_lang::sqlmodel::{
        ColumnDef, Dialect, Fidelity, IncompleteRegions, ModelSnapshot, ObjectBody, ObjectFragment,
        ObjectRef, Provenance, SqlName, TableBody,
    };
    let d = Dialect::SqlServer;
    let fragment = ObjectFragment {
        reference: ObjectRef::new(
            None,
            Some(SqlName::new("HR", d, false)),
            SqlName::new("WorkSite", d, false),
        ),
        kind: sqlly_lang::sqlmodel::ObjectKind::Table,
        dialect: d,
        body: ObjectBody::Table(TableBody {
            columns: ["SiteId", "Name"]
                .iter()
                .enumerate()
                .map(|(ordinal, col)| ColumnDef {
                    name: SqlName::new(*col, d, false),
                    ty: None,
                    nullable: Some(ordinal != 0),
                    default: None,
                    identity: ordinal == 0,
                    generated: None,
                    collation: None,
                    ordinal,
                })
                .collect(),
            ..TableBody::default()
        }),
        provenance: Provenance::Database {
            profile: "p1".into(),
        },
        fidelity: Fidelity::Exact,
        references: vec![],
    };
    let (snap, _) = ModelSnapshot::empty().apply(
        1,
        vec![fragment],
        &std::collections::BTreeSet::new(),
        IncompleteRegions::default(),
    );
    std::sync::Arc::new(snap)
}

#[gpui::test]
fn identity_lint_warns_and_survives_validation_pushes(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_schema_snapshot(identity_snapshot(), cx);
        editor.set_text("INSERT INTO HR.WorkSite VALUES (1, 'palms', '', 1, 1)", cx);
        editor.recompute_identity_lint_now(cx);
        let lint: Vec<_> = editor
            .diagnostics
            .iter()
            .filter(|d| d.source == DiagnosticSource::Lint)
            .cloned()
            .collect();
        assert_eq!(lint.len(), 1);
        assert_eq!(lint[0].severity, DiagnosticSeverity::Warning);
        assert!(lint[0].message.contains("IDENTITY_INSERT is OFF"));
        assert_eq!(
            &editor.content[lint[0].start_byte..lint[0].end_byte],
            "HR.WorkSite"
        );

        // A validation push keeps the lint entry; an execution error does
        // not duplicate it.
        editor.set_diagnostics(
            vec![ValidationDiagnostic {
                severity: DiagnosticSeverity::Error,
                source: DiagnosticSource::Validation,
                message: "boom".into(),
                line: 0,
                identifier: None,
                start_byte: 0,
                end_byte: 1,
                explainer: None,
                fix: None,
            }],
            cx,
        );
        assert_eq!(editor.diagnostics.len(), 2);
        editor.set_execution_error(
            ValidationDiagnostic {
                severity: DiagnosticSeverity::Error,
                source: DiagnosticSource::Execution,
                message: "run failed".into(),
                line: 0,
                identifier: None,
                start_byte: 0,
                end_byte: 1,
                explainer: None,
                fix: None,
            },
            cx,
        );
        assert_eq!(
            editor
                .diagnostics
                .iter()
                .filter(|d| d.source == DiagnosticSource::Lint)
                .count(),
            1
        );
        assert_eq!(editor.diagnostics.len(), 3);

        // SET IDENTITY_INSERT ON above the statement clears the warning.
        editor.set_text(
            "SET IDENTITY_INSERT HR.WorkSite ON;\nINSERT INTO HR.WorkSite VALUES (1, 'palms')",
            cx,
        );
        editor.recompute_identity_lint_now(cx);
        assert!(
            !editor
                .diagnostics
                .iter()
                .any(|d| d.source == DiagnosticSource::Lint)
        );
    });
}

#[gpui::test]
fn insert_hints_widen_horizontal_scroll_range(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "INSERT INTO orders VALUES (1, 'a')";
    view.update(cx, |editor, cx| {
        editor.set_text(sql, cx);
        editor.recompute_insert_hints_now();
        let without_hints = editor.max_display_cols(4);
        assert_eq!(without_hints, sql.chars().count());

        editor.set_completion_catalog(
            Arc::new(CompletionCatalog {
                objects: vec![CompletionObject::table(
                    "dbo",
                    "orders",
                    vec!["id".into(), "customer".into()],
                )],
                ..Default::default()
            }),
            cx,
        );
        editor.recompute_insert_hints_now();
        let with_hints = editor.max_display_cols(4);
        // Each label paints as "label + one space" inside the line.
        assert_eq!(
            with_hints,
            sql.chars().count() + "id: ".len() + "customer: ".len(),
            "content width must include the ghost labels or hinted lines clip"
        );
    });
}

#[gpui::test]
fn caret_column_ignores_inlay_bytes(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "INSERT INTO orders VALUES (1, 'a')";
    view.update(cx, |editor, cx| {
        editor.set_completion_catalog(
            Arc::new(CompletionCatalog {
                objects: vec![CompletionObject::table(
                    "dbo",
                    "orders",
                    vec!["id".into(), "customer".into()],
                )],
                ..Default::default()
            }),
            cx,
        );
        editor.set_text(sql, cx);
        editor.recompute_insert_hints_now();
        let after_one = "INSERT INTO orders VALUES (1".len();
        editor.cursor = after_one;
        assert_eq!(editor.cursor, after_one);
        assert_eq!(
            &editor.content[..editor.cursor],
            "INSERT INTO orders VALUES (1"
        );
        assert!(
            editor.content.is_char_boundary(editor.cursor),
            "caret must stay on a content byte, not a display-only inlay byte"
        );
    });
}

fn twelve_line_fold_sql() -> String {
    let mut lines = vec!["BEGIN".to_string()];
    for i in 1..=10 {
        lines.push(format!("  SELECT {i};"));
    }
    lines.push("END".to_string());
    lines.join("\n")
}

#[gpui::test]
fn fold_preview_set_when_pointer_over_chip(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update_in(cx, |editor, window, cx| {
        editor.set_text(twelve_line_fold_sql(), cx);
        editor.toggle_fold_at_line(0, cx);
        let chip = gpui::Bounds {
            origin: gpui::Point::new(px(120.0), px(4.0)),
            size: gpui::size(px(20.0), px(16.0)),
        };
        editor.fold_chip_bounds.borrow_mut().push((0, chip));
        editor.handle_mouse_move(
            &MouseMoveEvent {
                position: gpui::Point::new(px(130.0), px(12.0)),
                ..Default::default()
            },
            window,
            cx,
        );
        assert_eq!(editor.fold_preview, Some(0));
    });
}

#[gpui::test]
fn fold_preview_clears_when_pointer_leaves(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update_in(cx, |editor, window, cx| {
        editor.set_text(twelve_line_fold_sql(), cx);
        editor.toggle_fold_at_line(0, cx);
        let chip = gpui::Bounds {
            origin: gpui::Point::new(px(120.0), px(4.0)),
            size: gpui::size(px(20.0), px(16.0)),
        };
        editor.fold_chip_bounds.borrow_mut().push((0, chip));
        editor.handle_mouse_move(
            &MouseMoveEvent {
                position: gpui::Point::new(px(130.0), px(12.0)),
                ..Default::default()
            },
            window,
            cx,
        );
        assert_eq!(editor.fold_preview, Some(0));
        editor.handle_mouse_move(
            &MouseMoveEvent {
                position: gpui::Point::new(px(0.0), px(0.0)),
                ..Default::default()
            },
            window,
            cx,
        );
        assert!(editor.fold_preview.is_none());
    });
}

#[gpui::test]
fn comment_states_cover_every_line_and_follow_the_dialect(cx: &mut gpui::TestAppContext) {
    use sqlly_lang::sqlmodel::DatabaseType;

    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("SELECT 1\n/* open\nstill open\nclosed */\nSELECT 2", cx);
        let depths: Vec<u16> = editor.comment_states().iter().map(|s| s.depth).collect();
        assert_eq!(depths, vec![0, 0, 1, 1, 0]);
        // Lines past the end fall back to "not in a comment" rather than panic.
        assert!(!editor.comment_state_at(99).is_inside());

        // `#` is a MySQL comment and a T-SQL temp table, so the cached states
        // must be recomputed when the connection's engine changes.
        editor.set_text("# note\nSELECT 1", cx);
        assert!(!editor.comment_state_at(1).is_inside());
        editor.set_database_type(DatabaseType::MySql, cx);
        let (tokens, _) = crate::sql_syntax::classify_line(
            editor.database_type().family(),
            "# note",
            editor.comment_state_at(0),
        );
        assert_eq!(
            tokens.first().map(|t| t.kind),
            Some(crate::sql_syntax::SyntaxTokenKind::Comment),
        );
    });
}

/// A grouped query selecting an ungrouped column gets a live Lint underline
/// on the column with an explainer and a "Fix this" link; applying the fix
/// writes the GROUP BY as one undoable edit and clears the lint.
#[gpui::test]
fn group_by_lint_underlines_and_fixes(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        let sql = "SELECT\n    Category,\n    COUNT(1)\nfrom\n    Shared.Settings\n";
        editor.set_text(sql, cx);
        editor.recompute_identity_lint_now(cx);
        let lint: Vec<_> = editor
            .diagnostics
            .iter()
            .filter(|d| d.source == DiagnosticSource::Lint)
            .cloned()
            .collect();
        assert_eq!(lint.len(), 1);
        assert_eq!(&editor.content[lint[0].start_byte..lint[0].end_byte], "Category");
        assert!(lint[0].explainer.is_some());
        assert_eq!(
            lint[0].fix.as_ref().map(DiagnosticFix::label),
            Some("Fix this: add Category to GROUP BY")
        );

        editor.apply_diagnostic_fix(&lint[0], cx);
        assert_eq!(
            editor.content.as_ref(),
            "SELECT\n    Category,\n    COUNT(1)\nfrom\n    Shared.Settings\ngroup by\n    Category\n"
        );
        editor.recompute_identity_lint_now(cx);
        assert!(
            !editor
                .diagnostics
                .iter()
                .any(|d| d.source == DiagnosticSource::Lint)
        );

        editor.undo(cx);
        assert_eq!(editor.content.as_ref(), sql);
    });
}

fn directive_diag(sql: &str) -> ValidationDiagnostic {
    let d = crate::sqlly_directive::directive_diagnostics(sql)
        .into_iter()
        .next()
        .expect("a directive diagnostic");
    ValidationDiagnostic {
        severity: DiagnosticSeverity::Warning,
        source: DiagnosticSource::Validation,
        message: d.message,
        line: 0,
        identifier: None,
        start_byte: d.start,
        end_byte: d.end,
        explainer: None,
        fix: None,
    }
}

#[gpui::test]
fn diagnostic_on_a_directive_hides_the_directive_info_popup(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update_in(cx, |editor, window, cx| {
        // Valid directive grammar the format parser still rejects (no column).
        let sql = "-- sqlly format: colum=Total; number=2";
        editor.set_text(sql, cx);
        editor.set_diagnostics(vec![directive_diag(sql)], cx);
        editor.last_bounds = Some(gpui::Bounds {
            origin: gpui::Point::new(px(0.0), px(0.0)),
            size: gpui::size(px(800.0), px(200.0)),
        });
        let x = f32::from(editor.line_number_width) + 30.0;
        let y = f32::from(editor.line_height) * 0.5;
        editor.handle_mouse_move(
            &MouseMoveEvent {
                position: gpui::Point::new(px(x), px(y)),
                ..Default::default()
            },
            window,
            cx,
        );
        assert_eq!(editor.hovered_diagnostic, Some(0));
        assert!(
            editor.hovered_sqlly_directive.is_none(),
            "the info popup must not cover the diagnostic"
        );
    });
}

#[gpui::test]
fn pointer_leaving_the_editor_drops_every_hover_popup(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_text("-- sqlly view: grid\nSELECT 1", cx);
        editor.hovered_diagnostic = Some(0);
        editor.hovered_sqlly_directive = Some((0, 19));
        editor.hovered_play_line = Some(1);
        editor.fold_preview = Some(0);
        editor.last_mouse_position = Some(gpui::Point::new(px(10.0), px(10.0)));
        editor
            .directive_hint_bounds
            .set(Some(gpui::Bounds::default()));
        editor
            .diagnostic_popup_bounds
            .set(Some(gpui::Bounds::default()));
        editor.handle_pointer_left(cx);
        assert!(editor.hovered_diagnostic.is_none());
        assert!(editor.hovered_sqlly_directive.is_none());
        assert!(editor.hovered_play_line.is_none());
        assert!(editor.fold_preview.is_none());
        assert!(editor.hover_info.is_none());
        assert!(editor.hover_dwell_task.is_none());
        assert!(editor.last_mouse_position.is_none());
        assert!(editor.directive_hint_bounds.get().is_none());
        assert!(editor.diagnostic_popup_bounds.get().is_none());
    });
}

/// Feedback ifb_3edd: accepting a table after `schema.` (`dbo.su▏`, which
/// inserts only the bare name) must still add the alias, derived by the
/// chosen alias rule, and must not when auto-add is off.
#[test]
fn schema_dot_table_accept_adds_the_alias_per_the_alias_rule() {
    let sql = "--note\nSELECT\n\t*\nfrom\n\tdbo.su";
    let item = plain_table_completion("dbo.Subscriptions", "Subscriptions");
    let with =
        |alias_generation, include_schema_in_alias, auto_add_table_alias| EditorPreferences {
            alias_generation,
            include_schema_in_alias,
            auto_add_table_alias,
            ..EditorPreferences::default()
        };
    let cases = [
        (
            with(AliasGeneration::CapitalLettersLowercase, false, true),
            "Subscriptions AS s",
        ),
        (
            with(AliasGeneration::CapitalLettersLowercase, true, true),
            "Subscriptions AS ds",
        ),
        (
            with(AliasGeneration::AutoBasedOnJoin, false, true),
            "Subscriptions AS s",
        ),
        (
            with(AliasGeneration::CapitalLettersLowercase, false, false),
            "Subscriptions",
        ),
        (
            with(AliasGeneration::DoNotSuggest, false, true),
            "Subscriptions",
        ),
    ];
    for (preferences, expected) in cases {
        assert_eq!(
            accepted_completion_insert_text(&item, &preferences, sql, sql.len()),
            expected,
            "{preferences:?}"
        );
    }
}

/// Feedback ifb_3edd: "auto based on join" numbers the first letter when it
/// is already taken, and the second-suggestion mode offers the alias after a
/// schema-dot accept too.
#[test]
fn schema_dot_join_accept_numbers_a_taken_alias_and_offers_it_on_second_accept() {
    let sql = "SELECT *\nFROM dbo.Settings s\nJOIN dbo.su";
    let item = plain_table_completion("dbo.Subscriptions", "Subscriptions");
    let auto = EditorPreferences {
        alias_generation: AliasGeneration::AutoBasedOnJoin,
        ..EditorPreferences::default()
    };
    assert_eq!(
        accepted_completion_insert_text(&item, &auto, sql, sql.len()),
        "Subscriptions AS s2"
    );
    let second = EditorPreferences {
        auto_add_table_alias: false,
        ..EditorPreferences::default()
    };
    let (alias, table) =
        suggested_table_alias(&item, &second, sql, sql.len()).expect("alias offered");
    assert_eq!((alias.as_str(), table.as_str()), ("s2", "Subscriptions"));
}

#[gpui::test]
fn escape_keystroke_closes_function_signature_help(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        cx.bind_keys([gpui::KeyBinding::new("escape", Dismiss, Some("SqlEditor"))]);
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    view.update(cx, |e, cx| {
        e.set_text("SELECT DATEADD(", cx);
        e.cursor = e.content.len();
        e.refresh_signature_help(cx);
    });
    cx.run_until_parked();
    assert!(
        view.read_with(cx, |e, _| e.signature_help.is_some()),
        "typing into DATEADD( shows signature help"
    );
    cx.simulate_keystrokes("escape");
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    view.read_with(cx, |e, _| {
        assert!(e.signature_help.is_none(), "Esc closes signature help");
        assert_eq!(e.text(), "SELECT DATEADD(");
    });
}

#[gpui::test]
fn one_escape_closes_signature_help_and_its_argument_popup(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        cx.bind_keys([gpui::KeyBinding::new("escape", Dismiss, Some("SqlEditor"))]);
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        let mut editor = SqlEditor::new(cx);
        editor.set_services(Services::new(), cx);
        editor.focus_handle.focus(window, cx);
        editor
    });
    cx.simulate_input("SELECT DATEADD(");
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    view.read_with(cx, |e, _| {
        assert!(
            e.signature_help.is_some(),
            "typing into DATEADD( shows signature help"
        );
        assert!(
            e.completion_visible,
            "and the datepart value popup under it"
        );
    });
    cx.simulate_keystrokes("escape");
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    view.read_with(cx, |e, _| {
        assert!(e.signature_help.is_none(), "Esc closes signature help");
        assert!(
            !e.completion_visible,
            "the same Esc closes its argument popup"
        );
        assert_eq!(e.text(), "SELECT DATEADD(");
    });
}

/// The bold text of each run, for asserting which words draw bold.
fn bold_words(
    line: &str,
    bold: &crate::preferences::SyntaxBoldPreferences,
    names: &crate::sql_syntax::SemanticNames,
) -> Vec<String> {
    let font = gpui::font("monospace");
    let runs = SqlEditor::syntax_runs(
        line,
        &font,
        sqlly_lang::sqlmodel::Dialect::SqlServer,
        crate::sql_syntax::CommentState::default(),
        bold,
        names,
    );
    assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), line.len());
    let mut offset = 0;
    let mut words = Vec::new();
    for run in runs {
        if run.font.weight == FontWeight::BOLD {
            words.push(line[offset..offset + run.len].to_string());
        }
        offset += run.len;
    }
    words
}

#[test]
fn keywords_literals_and_functions_are_bold_by_default() {
    let names = crate::sql_syntax::SemanticNames::from_catalog(&CompletionCatalog {
        objects: vec![CompletionObject::table("dbo", "Orders", Vec::new())],
        ..Default::default()
    });
    let line = "SELECT COUNT(1), 'x', NULL, CAST(2 AS INT) FROM dbo.Orders";
    let defaults = crate::preferences::SyntaxBoldPreferences::default();
    assert_eq!(
        bold_words(line, &defaults, &names),
        [
            "SELECT", "COUNT", "1", "'x'", "NULL", "CAST", "2", "AS", "FROM"
        ]
    );

    let opted_in = crate::preferences::SyntaxBoldPreferences {
        keywords: false,
        literals: false,
        functions: false,
        system_types: true,
        schemas: true,
        tables: true,
        ..defaults
    };
    assert_eq!(
        bold_words(line, &opted_in, &names),
        ["INT", "dbo", "Orders"]
    );
}

#[test]
fn format_as_you_go_reformats_the_statement_and_keeps_the_caret() {
    let prefs = crate::preferences::SqlFormattingPreferences::default();
    let indent = crate::sql_formatter::EditorIndent {
        style: IndentStyle::Spaces,
        width: 4,
    };
    let content = "select a,b from t where a=1";
    let cursor = content.find(" from").unwrap();
    let (range, formatted, caret) =
        format_as_you_go_edit(content, (0, content.len()), cursor, &prefs, indent)
            .expect("unformatted SQL changes");
    assert_eq!(range, 0..content.len());
    assert_ne!(formatted, content);
    // The caret still follows the `b` it was after.
    assert_eq!(&formatted[..caret].chars().last(), &Some('b'));
    // Already-formatted text and fragments are left alone.
    assert!(format_as_you_go_edit(&formatted, (0, formatted.len()), 0, &prefs, indent).is_none());
    assert!(format_as_you_go_edit("a = 1", (0, 5), 5, &prefs, indent).is_none());
}

#[test]
fn format_as_you_go_keeps_the_caret_past_quotes_always_quote_adds() {
    let prefs = crate::preferences::SqlFormattingPreferences {
        identifier_quoting: sqlly_lang::sqlformat::IdentifierQuoting::AlwaysQuote,
        ..Default::default()
    };
    let content = "select a from t";
    let (_, formatted, caret) = format_as_you_go_edit(
        content,
        (0, content.len()),
        content.len(),
        &prefs,
        fmt_indent(),
    )
    .expect("quoting changes the text");
    assert!(formatted.contains('"'), "{formatted}");
    // The caret sat after `t`; it lands after the closing quote the
    // formatter wrapped around it, so Enter does not split `"t"`.
    assert_eq!(caret, formatted.len(), "{formatted:?}");
    // Mid-statement: after `a` in the select list, past its closing quote.
    let cursor = content.find(" from").unwrap();
    let (_, formatted, caret) =
        format_as_you_go_edit(content, (0, content.len()), cursor, &prefs, fmt_indent()).unwrap();
    assert!(
        formatted[..caret].ends_with("\"a\""),
        "{formatted:?} {caret}"
    );
}

#[test]
fn format_as_you_go_keeps_the_caret_when_quotes_are_removed() {
    let prefs = crate::preferences::SqlFormattingPreferences {
        identifier_quoting: sqlly_lang::sqlformat::IdentifierQuoting::RemoveUnnecessaryQuotes,
        ..Default::default()
    };
    let content = "select [a], [b] from [t] where [a]=1";
    let cursor = content.find(" from").unwrap();
    let (_, formatted, caret) =
        format_as_you_go_edit(content, (0, content.len()), cursor, &prefs, fmt_indent())
            .expect("quotes are dropped");
    assert!(!formatted.contains('['), "{formatted}");
    assert!(formatted[..caret].ends_with('b'), "{formatted:?} {caret}");
    // At the end of the statement the caret stays at the end.
    let (_, formatted, caret) = format_as_you_go_edit(
        content,
        (0, content.len()),
        content.len(),
        &prefs,
        fmt_indent(),
    )
    .unwrap();
    assert_eq!(caret, formatted.len());
}

#[test]
fn format_as_you_go_keeps_the_caret_across_added_parentheses() {
    let prefs = crate::preferences::SqlFormattingPreferences {
        parenthesize_mixed_boolean_logic: true,
        // A short query kept on one line is not re-grouped.
        keep_short_queries_on_one_line: false,
        ..Default::default()
    };
    let content = "select x from t where a=1 and b=2 or c=3";
    // Caret after `b=2`: the formatter closes a parenthesis right there and
    // the caret steps over it, so Enter lands outside the group.
    let cursor = content.find(" or").unwrap();
    let (_, formatted, caret) =
        format_as_you_go_edit(content, (0, content.len()), cursor, &prefs, fmt_indent())
            .expect("parentheses are added");
    assert!(formatted.contains('('), "{formatted}");
    assert!(
        formatted[..caret].ends_with("b = 2)"),
        "{formatted:?} {caret}"
    );
    // Caret at the end stays at the end.
    let (_, formatted, caret) = format_as_you_go_edit(
        content,
        (0, content.len()),
        content.len(),
        &prefs,
        fmt_indent(),
    )
    .unwrap();
    assert_eq!(caret, formatted.len());
    // A parenthesis the user typed stays ahead of the caret.
    let typed = "select x from t where (a=1 and b=2) or c=3";
    let cursor = typed.find(") or").unwrap();
    let (_, formatted, caret) =
        format_as_you_go_edit(typed, (0, typed.len()), cursor, &prefs, fmt_indent())
            .expect("spacing changes");
    assert!(
        formatted[..caret].ends_with("b = 2"),
        "{formatted:?} {caret}"
    );
    assert!(formatted[caret..].starts_with(')'), "{formatted:?} {caret}");
}

#[test]
fn format_as_you_go_maps_a_mid_statement_caret_by_meaningful_characters() {
    let prefs = crate::preferences::SqlFormattingPreferences::default();
    let content = "select   a,b   from t where a=1";
    // (text the caret sits right after, the character it must still follow)
    for (prefix, expect_before) in [
        ("select   a", 'a'),
        ("select   a,b", 'b'),
        ("select   a,b   ", 'b'),
        ("from t", 't'),
        ("where a", 'a'),
    ] {
        let cursor = content.find(prefix).unwrap() + prefix.len();
        let (_, formatted, caret) =
            format_as_you_go_edit(content, (0, content.len()), cursor, &prefs, fmt_indent())
                .unwrap();
        assert_eq!(
            formatted[..caret].chars().last(),
            Some(expect_before),
            "{prefix:?}: {formatted:?} caret {caret}"
        );
    }
}

#[test]
fn format_as_you_go_skips_a_caret_below_the_statement() {
    let prefs = crate::preferences::SqlFormattingPreferences::default();
    // Enter on the blank line after a statement must not reach back up,
    // even though the statement index maps trailing offsets to it.
    let content = "select a from t\n\n";
    assert!(format_as_you_go_edit(content, (0, 15), content.len(), &prefs, fmt_indent()).is_none());
    assert!(format_as_you_go_edit(content, (0, 15), 16, &prefs, fmt_indent()).is_none());
    // The span handed in may include the trailing whitespace; the caret at
    // the statement's last character still formats.
    assert!(format_as_you_go_edit(content, (0, content.len()), 15, &prefs, fmt_indent()).is_some());
}

fn fmt_indent() -> crate::sql_formatter::EditorIndent {
    crate::sql_formatter::EditorIndent {
        style: IndentStyle::Spaces,
        width: 4,
    }
}

#[test]
fn join_completion_puts_on_on_a_new_line_when_configured() {
    let prefs = crate::preferences::SqlFormattingPreferences {
        on_clause_placement: sqlly_lang::sqlformat::OnClausePlacement::OnNewLine,
        ..Default::default()
    };
    let content = "SELECT ol.OrderNo, ol.ProductId, ol.Quantity FROM dbo.OrderLines ol JOIN ";
    let insert = "dbo.Orders o ON o.OrderNo = ol.OrderNo";
    let out = format_insert_in_context(content, 0, content.len(), insert, "", &prefs, fmt_indent())
        .expect("ON moves to its own line");
    let (head, tail) = out.split_once('\n').expect("a line break before ON");
    assert_eq!(head.trim(), "dbo.Orders o");
    assert!(
        tail.trim_start().starts_with("ON o.OrderNo = ol.OrderNo"),
        "{out:?}"
    );
}

#[test]
fn keyword_completion_follows_keyword_case() {
    let prefs = crate::preferences::SqlFormattingPreferences {
        keyword_case: sqlly_lang::sqlformat::KeywordCase::Lowercase,
        ..Default::default()
    };
    let content = "SELECT a FROM t ";
    assert_eq!(
        format_insert_in_context(
            content,
            0,
            content.len(),
            "WHERE ",
            "",
            &prefs,
            fmt_indent()
        )
        .as_deref(),
        Some("where ")
    );
}

#[test]
fn completion_formatting_leaves_fragments_and_unchanged_text_alone() {
    let prefs = crate::preferences::SqlFormattingPreferences::default();
    // Not a statement: no formatting context.
    assert!(format_insert_in_context("a = ", 0, 4, "b", "", &prefs, fmt_indent()).is_none());
    // Already laid out the configured way.
    let content = "SELECT a FROM ";
    assert!(
        format_insert_in_context(content, 0, content.len(), "t", "", &prefs, fmt_indent())
            .is_none()
    );
}

#[test]
fn completion_formatting_sees_the_rest_of_the_statement() {
    // Short enough to stay on one line up to the caret; the text after the
    // caret makes the statement long enough for the JOIN layout to apply.
    let prefs = crate::preferences::SqlFormattingPreferences {
        on_clause_placement: sqlly_lang::sqlformat::OnClausePlacement::OnNewLine,
        ..Default::default()
    };
    let content = "SELECT * FROM dbo.OrderLines ol JOIN ";
    let insert = "dbo.Orders o ON o.OrderNo = ol.OrderNo";
    assert!(
        format_insert_in_context(content, 0, content.len(), insert, "", &prefs, fmt_indent())
            .is_none(),
        "a short query stays on one line"
    );
    let rest = " WHERE ol.Quantity > 100 AND ol.ProductId IN (1, 2, 3)";
    let out = format_insert_in_context(
        content,
        0,
        content.len(),
        insert,
        rest,
        &prefs,
        fmt_indent(),
    )
    .expect("the full statement is long");
    assert!(out.contains('\n'), "{out:?}");
}

#[gpui::test]
fn word_movement_follows_the_camel_hump_setting(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| e.set_text("SELECT SalesOrderLineId FROM t", cx));
    let word_right = |view: &gpui::Entity<SqlEditor>, cx: &mut gpui::VisualTestContext| {
        view.update_in(cx, |e, window, cx| {
            e.handle_move_word_right(&MoveWordRight, window, cx);
            e.cursor
        })
    };

    // Off (the default): the whole identifier is one stop.
    view.update(cx, |e, _| e.cursor = 7);
    assert_eq!(word_right(&view, cx), 24);

    // On: each capital letter inside the word is a stop.
    view.update(cx, |e, cx| {
        let prefs = EditorPreferences {
            camel_hump_word_movement: true,
            ..Default::default()
        };
        e.set_editor_prefs(prefs, cx);
        e.cursor = 7;
    });
    let stops: Vec<usize> = (0..4).map(|_| word_right(&view, cx)).collect();
    assert_eq!(stops, [12, 17, 21, 23]);
}

#[gpui::test]
fn repeated_tab_and_shift_tab_keep_indenting_a_multi_line_selection(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        e.set_text("SELECT a\nFROM t\nWHERE x = 1", cx);
        e.editor_prefs.indent_style = IndentStyle::Spaces;
        e.editor_prefs.spaces_per_tab_raw = 2;
        e.selection_anchor = Some(0);
        e.cursor = e.content.len();
    });
    let tab = |view: &gpui::Entity<SqlEditor>, cx: &mut gpui::VisualTestContext| {
        view.update_in(cx, |e, window, cx| e.handle_tab(&Tab, window, cx));
    };
    let shift_tab = |view: &gpui::Entity<SqlEditor>, cx: &mut gpui::VisualTestContext| {
        view.update_in(cx, |e, window, cx| {
            e.handle_shift_tab(&ShiftTab, window, cx)
        });
    };

    tab(&view, cx);
    // A popup somehow open over the block must not swallow the next Tab.
    view.update(cx, |e, _| e.completion_visible = true);
    tab(&view, cx);
    view.read_with(cx, |e, _| {
        assert_eq!(
            e.content.as_ref(),
            "    SELECT a\n    FROM t\n    WHERE x = 1"
        );
        assert!(e.selection().is_some(), "the block stays selected");
        assert!(!e.completion_visible, "no popup after a block indent");
    });

    shift_tab(&view, cx);
    tab(&view, cx);
    tab(&view, cx);
    view.read_with(cx, |e, _| {
        assert_eq!(
            e.content.as_ref(),
            "      SELECT a\n      FROM t\n      WHERE x = 1"
        );
        assert!(e.selection().is_some());
    });
}

#[gpui::test]
fn context_menu_carries_the_standard_editing_commands(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let commands = |e: &SqlEditor| -> Vec<(&'static str, bool, bool)> {
        StandardContextItem::flatten(e.standard_context_items())
            .into_iter()
            .filter_map(|item| match item {
                StandardContextItem::Command {
                    label,
                    enabled,
                    action,
                    ..
                } => Some((label, enabled, action.is_some())),
                _ => None,
            })
            .collect()
    };
    view.update(cx, |e, cx| e.set_text("select 1", cx));
    view.read_with(cx, |e, _| {
        let items = commands(e);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| *l).collect();
        for expected in [
            "Go to Definition",
            "Run Statement",
            "Cut Line",
            "Copy Line",
            "Paste",
            "Select All",
            "Undo",
            "Redo",
            "Find",
            "Replace",
            "Change All Occurrences",
            "Trigger Suggest",
            "Format Document",
            "Format Selection",
            "Toggle Line Comment",
            "Fold All",
            "Unfold All",
        ] {
            assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
        }
        let enabled = |label| items.iter().find(|(l, _, _)| *l == label).unwrap().1;
        // Nothing selected: Cut / Copy act on the current line like their
        // shortcuts do, so they stay enabled and say so; Format Selection
        // is greyed out.
        assert!(enabled("Cut Line") && enabled("Copy Line"));
        assert!(!enabled("Format Selection"));
        assert!(enabled("Paste") && enabled("Format Document"));
        // Every command that mirrors a keyboard action carries it, so the
        // menu can show the shortcut; the two event-emitting ones do not.
        for (label, _, has_action) in &items {
            let expect = !matches!(*label, "Go to Definition" | "Run Statement");
            assert_eq!(*has_action, expect, "{label}");
        }
    });
    view.update(cx, |e, _| {
        e.selection_anchor = Some(0);
        e.cursor = 6;
    });
    view.read_with(cx, |e, _| {
        let items = commands(e);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| *l).collect();
        assert!(
            labels.contains(&"Cut") && labels.contains(&"Copy"),
            "{labels:?}"
        );
        assert!(
            items
                .iter()
                .all(|(l, on, _)| !matches!(*l, "Cut" | "Copy" | "Format Selection") || *on)
        );
    });
    // An empty buffer has no line to cut or copy.
    view.update(cx, |e, cx| e.set_text("", cx));
    view.read_with(cx, |e, _| {
        let items = commands(e);
        let enabled = |label| items.iter().find(|(l, _, _)| *l == label).unwrap().1;
        assert!(!enabled("Cut Line") && !enabled("Copy Line"));
    });
}

#[gpui::test]
fn format_as_you_go_enter_is_one_undo_step_with_the_caret_restored(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let original = "select a,b from t where a=1";
    view.update(cx, |e, cx| {
        e.set_text(original, cx);
        e.format_as_you_go_override = Some(true);
        e.cursor = original.len();
        e.selection_anchor = None;
        e.accept_suggestion_or_insert_newline(cx);
    });
    view.read_with(cx, |e, _| {
        let text = e.text();
        assert_ne!(text.trim_end(), original, "{text:?}");
        // The new line keeps the indent of the (now indented) WHERE
        // condition line above it.
        assert!(
            text.trim_end_matches([' ', '\t']).ends_with('\n'),
            "newline inserted: {text:?}"
        );
        assert_eq!(e.cursor, text.len(), "caret on the new line");
        assert!(!e.has_selection());
    });
    // One Ctrl+Z: the exact pre-Enter text and caret, nothing selected.
    view.update(cx, |e, cx| e.undo(cx));
    view.read_with(cx, |e, _| {
        assert_eq!(e.text(), original);
        assert_eq!(e.cursor, original.len());
        assert!(!e.has_selection());
    });
    // Redo re-applies reformat and newline together.
    view.update(cx, |e, cx| e.redo(cx));
    view.read_with(cx, |e, _| {
        let text = e.text();
        assert!(
            text.trim_end_matches([' ', '\t']).ends_with('\n'),
            "{text:?}"
        );
        assert_eq!(e.cursor, text.len());
    });
    // With the setting off, Enter is a plain newline.
    view.update(cx, |e, cx| {
        e.set_text(original, cx);
        e.format_as_you_go_override = Some(false);
        e.cursor = original.len();
        e.accept_suggestion_or_insert_newline(cx);
    });
    view.read_with(cx, |e, _| assert_eq!(e.text(), format!("{original}\n")));
    // Enter on the blank line below a statement leaves the statement alone.
    view.update(cx, |e, cx| {
        e.set_text(format!("{original}\n"), cx);
        e.format_as_you_go_override = Some(true);
        e.cursor = original.len() + 1;
        e.accept_suggestion_or_insert_newline(cx);
    });
    view.read_with(cx, |e, _| assert_eq!(e.text(), format!("{original}\n\n")));
}

/// Natural-language dates: Ctrl+Shift+Space after a date comparison reads a
/// phrase, Enter writes the literal or range plus its `--sqlly date:` marker.
#[gpui::test]
fn date_helper_writes_a_range_with_its_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "SELECT * FROM Orders WHERE OrderDate = november";
    view.update(cx, |e, cx| {
        e.set_text(sql, cx);
        e.cursor = sql.len();
        assert!(
            e.open_date_helper(cx),
            "a date-named column opens the helper"
        );
        assert!(e.date_helper_card().is_some());
        assert!(e.accept_date_helper(cx));
    });
    view.read_with(cx, |e, _| {
        let text = e.text();
        assert!(text.contains("OrderDate BETWEEN '"), "{text}");
        assert!(text.ends_with(" --sqlly date:november"), "{text}");
        assert!(e.date_helper.is_none());
        assert_eq!(e.date_markers().len(), 1);
    });
}

#[gpui::test]
fn date_helper_moves_the_rest_of_the_line_below_its_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let before = "WHERE OrderDate >= today";
    let sql = format!("{before} AND Status = 1");
    view.update(cx, |e, cx| {
        e.set_text(&sql, cx);
        e.cursor = before.len();
        assert!(e.open_date_helper(cx));
        assert!(e.accept_date_helper(cx));
    });
    view.read_with(cx, |e, _| {
        let text = e.text();
        assert!(
            text.contains("--sqlly date:today\nAND Status = 1"),
            "{text}"
        );
        assert!(text.starts_with("WHERE OrderDate >= '"), "{text}");
    });
}

#[gpui::test]
fn date_helper_stays_out_of_non_date_comparisons(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |e, cx| {
        let sql = "SELECT * FROM Orders WHERE Status = ";
        e.set_text(sql, cx);
        e.cursor = sql.len();
        assert!(!e.open_date_helper(cx));
        // Turned off, even a date column falls back to value lookup.
        let sql = "SELECT * FROM Orders WHERE OrderDate = ";
        e.set_text(sql, cx);
        e.cursor = sql.len();
        e.editor_prefs.natural_dates.enabled = false;
        assert!(!e.open_date_helper(cx));
    });
}

#[gpui::test]
fn date_helper_keeps_an_unreadable_phrase_and_esc_closes_it(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "WHERE OrderDate = blorp";
    view.update(cx, |e, cx| {
        e.set_text(sql, cx);
        e.cursor = sql.len();
        assert!(e.open_date_helper(cx));
        assert!(
            e.accept_date_helper(cx),
            "Enter is consumed so the phrase can be fixed"
        );
        assert_eq!(e.text(), sql);
        assert!(e.date_helper.is_some());
        e.dismiss_transient_ui(cx);
        assert!(e.date_helper.is_none());
    });
}

#[gpui::test]
fn refreshing_dates_rewrites_stale_literals(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    let sql = "SELECT *\nFROM Orders\nWHERE OrderDate BETWEEN '2000-01-01T00:00:00' AND \
               '2000-01-01T23:59:59.997' --sqlly date:today\n  AND ShipDate < '2000-01-01T00:00:00' --sqlly date:today";
    view.update(cx, |e, cx| {
        e.set_text(sql, cx);
        e.cursor = 0;
        assert_eq!(e.statements_with_date_markers().len(), 1);
        assert_eq!(e.refresh_all_dates(cx), 2);
    });
    view.read_with(cx, |e, _| {
        let text = e.text();
        assert!(!text.contains("2000-01-01"), "{text}");
        assert_eq!(text.matches("--sqlly date:today").count(), 2);
        assert_eq!(e.cursor, 0, "the caret stays put");
    });
    // A second refresh the same day changes nothing.
    view.update(cx, |e, cx| assert_eq!(e.refresh_all_dates(cx), 0));
}

/// With services attached, typing schedules a debounced completion instead of
/// repainting synchronously. The editor is drawn from gpui's frame cache, so
/// the keystroke must notify the entity itself or the character only appears
/// when the 120 ms completion task lands.
#[gpui::test]
fn typing_with_services_notifies_immediately(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    view.update(cx, |editor, cx| {
        editor.set_services(Services::new(), cx);
    });
    cx.run_until_parked();
    let notified = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let _sub = cx.update(|_, cx| {
        let notified = notified.clone();
        cx.observe(&view, move |_, _| notified.set(notified.get() + 1))
    });
    view.update(cx, |editor, cx| editor.replace_selection("s", cx));
    // No clock advance: the debounced completion/hint tasks have not run.
    assert_eq!(
        view.read_with(cx, |editor, _| editor.text().to_string()),
        "s"
    );
    assert!(notified.get() > 0, "a keystroke must repaint the editor");
}

/// The scrollbar thumbs are sized in `render` from the previous layout's
/// scroll geometry. After an edit that changes the line count the editor must
/// re-render once more by itself, since no parent frame refreshes a cached view.
#[gpui::test]
fn scrollbar_geometry_catches_up_after_the_line_count_changes(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| SqlEditor::new(cx));
    cx.run_until_parked();
    view.update(cx, |editor, cx| {
        editor.set_text("select 1;\n".repeat(2_000), cx);
    });
    for _ in 0..5 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(16));
        cx.run_until_parked();
    }
    view.read_with(cx, |editor, _| {
        let max = editor.scroll_handle.max_offset();
        assert!(max.y > gpui::px(0.0), "the buffer overflows the viewport");
        assert_eq!(
            editor.scrollbar_geometry,
            (editor.scroll_handle.bounds().size, max),
            "the thumbs were rendered from the current scroll geometry"
        );
    });
}
