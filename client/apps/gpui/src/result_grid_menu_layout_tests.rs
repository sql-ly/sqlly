//! Guards for the results grid's context-menu layout: reorganising it
//! (grouping into submenus, reordering) must never drop or add an action, the
//! top level stays short, and no submenu is ever empty.

use super::*;
use sqlly_datatable::{CellValue as GridCell, ColumnContext, ColumnKind, ContextMenuSelection};
use sqlly_lang::sqlmodel::DatabaseType;
use std::path::Path;

fn cell_target(column_index: usize) -> ContextMenuTarget {
    ContextMenuTarget::Cell {
        display_row_index: 0,
        source_row_index: 0,
        column_index,
    }
}

fn columns(kinds: &[(&str, ColumnKind)]) -> Vec<ColumnContext> {
    kinds
        .iter()
        .enumerate()
        .map(|(index, (name, kind))| ColumnContext {
            index,
            name: (*name).into(),
            kind: *kind,
        })
        .collect()
}

fn result_columns(types: &[(&str, &str)]) -> Vec<ResultColumn> {
    types
        .iter()
        .map(|(name, data_type)| ResultColumn {
            name: (*name).into(),
            data_type: (*data_type).into(),
            nullable: true,
        })
        .collect()
}

/// A provider for one grid whose columns are `types` (name, engine type),
/// with the column snapshot the host passes, so the WHERE dry run is live.
fn provider(db: DatabaseType, types: &[(&str, &str)]) -> ResultGridMenuProvider {
    ResultGridMenuProvider::new(WeakEntity::new_invalid(), None, 0)
        .with_export_prefs(ExportPreferences::default())
        .with_source_db(Some(db))
        .with_column_types(types.iter().map(|(_, t)| (*t).to_string()).collect())
        .with_columns(result_columns(types), 0)
}

/// One single-row request over `values` with the clicked target.
fn request(
    target: ContextMenuTarget,
    selection: Option<ContextMenuSelection>,
    cols: &[(&str, ColumnKind)],
    values: Vec<GridCell>,
) -> ContextMenuRequest {
    ContextMenuRequest::for_test(target, selection, vec![values], columns(cols))
}

/// A crypto wallet address: text that merely starts with `0x`.
const WALLET: &str = "0x52908400098527886E0F7030069857D2E4169EE7";

/// Every context the grid opens a menu in, by name.
pub(super) fn scenarios() -> Vec<(&'static str, Vec<ContextMenuItem>)> {
    let int_text = [("id", "int"), ("name", "nvarchar")];
    let int_text_kinds = [("id", ColumnKind::Integer), ("name", ColumnKind::Text)];
    let row = || vec![GridCell::Integer(7), GridCell::Text("hello".into())];
    let mut out = Vec::new();

    out.push((
        "column header / int",
        provider(DatabaseType::SqlServer, &int_text).menu_items(&request(
            ContextMenuTarget::ColumnHeader { column_index: 0 },
            None,
            &int_text_kinds,
            row(),
        )),
    ));
    out.push((
        "column header / decimal",
        provider(DatabaseType::SqlServer, &[("price", "decimal(10,2)")]).menu_items(&request(
            ContextMenuTarget::ColumnHeader { column_index: 0 },
            None,
            &[("price", ColumnKind::Decimal)],
            vec![GridCell::Decimal(1.5)],
        )),
    ));
    let mut fk = std::collections::HashMap::new();
    fk.insert(
        0,
        FkLabelHeaderMenuInfo {
            local_column: "customer_id".into(),
            display_column: "name".into(),
            column_label_on: true,
            show_fk_labels: true,
        },
    );
    out.push((
        "column header / labelled foreign key",
        provider(DatabaseType::Postgres, &[("customer_id", "int4")])
            .with_fk_label_headers(fk)
            .menu_items(&request(
                ContextMenuTarget::ColumnHeader { column_index: 0 },
                None,
                &[("customer_id", ColumnKind::Integer)],
                vec![GridCell::Integer(3)],
            )),
    ));
    out.push((
        "cell / text",
        provider(DatabaseType::SqlServer, &int_text).menu_items(&request(
            cell_target(1),
            None,
            &int_text_kinds,
            row(),
        )),
    ));
    out.push((
        "cell / integer",
        provider(DatabaseType::SqlServer, &int_text).menu_items(&request(
            cell_target(0),
            None,
            &int_text_kinds,
            row(),
        )),
    ));
    out.push((
        "cell / integer via relay",
        provider(DatabaseType::Postgres, &[("id", "int4"), ("name", "text")])
            .with_relay_route(true)
            .menu_items(&request(cell_target(0), None, &int_text_kinds, row())),
    ));
    out.push((
        "cell / NULL",
        provider(DatabaseType::MySql, &int_text).menu_items(&request(
            cell_target(1),
            None,
            &int_text_kinds,
            vec![GridCell::Integer(7), GridCell::None],
        )),
    ));
    out.push((
        "cell / boolean",
        provider(DatabaseType::Postgres, &[("active", "bool")]).menu_items(&request(
            cell_target(0),
            None,
            &[("active", ColumnKind::Boolean)],
            vec![GridCell::Boolean(true)],
        )),
    ));
    out.push((
        "cell / JSON",
        provider(DatabaseType::Postgres, &[("doc", "jsonb")]).menu_items(&request(
            cell_target(0),
            None,
            &[("doc", ColumnKind::Text)],
            vec![GridCell::Text("{\"a\": 1}".into())],
        )),
    ));
    out.push((
        "cell / cut preview",
        provider(DatabaseType::Sqlite, &[("notes", "TEXT")]).menu_items(&request(
            cell_target(0),
            None,
            &[("notes", ColumnKind::Text)],
            vec![GridCell::Text("It's a long note … [2.4 MiB]".into())],
        )),
    ));
    out.push((
        "cell / binary",
        provider(
            DatabaseType::SqlServer,
            &[("id", "int"), ("hash", "varbinary")],
        )
        .menu_items(&request(
            cell_target(1),
            None,
            &[("id", ColumnKind::Integer), ("hash", ColumnKind::Text)],
            vec![GridCell::Integer(7), GridCell::Text("<16 bytes>".into())],
        )),
    ));
    out.push((
        "cell / hex-looking text",
        provider(DatabaseType::Postgres, &[("wallet", "text")]).menu_items(&request(
            cell_target(0),
            None,
            &[("wallet", ColumnKind::Text)],
            vec![GridCell::Text(WALLET.into())],
        )),
    ));
    out.push((
        "cell / masked",
        provider(DatabaseType::SqlServer, &int_text).menu_items(&request(
            cell_target(1),
            None,
            &int_text_kinds,
            vec![GridCell::Integer(7), GridCell::Text("••••abcd".into())],
        )),
    ));
    out.push((
        "multi-cell / one column",
        provider(DatabaseType::SqlServer, &int_text).menu_items(&ContextMenuRequest::for_test(
            cell_target(0),
            Some(ContextMenuSelection {
                row_start: 0,
                row_end: 1,
                column_start: 0,
                column_end: 0,
            }),
            vec![
                vec![GridCell::Integer(7), GridCell::Text("a".into())],
                vec![GridCell::Integer(8), GridCell::Text("b".into())],
            ],
            columns(&int_text_kinds),
        )),
    ));
    out.push((
        "multi-cell / rectangle",
        provider(DatabaseType::Postgres, &[("id", "int4"), ("name", "text")]).menu_items(
            &ContextMenuRequest::for_test(
                cell_target(0),
                Some(ContextMenuSelection {
                    row_start: 0,
                    row_end: 1,
                    column_start: 0,
                    column_end: 1,
                }),
                vec![
                    vec![GridCell::Integer(7), GridCell::Text("a".into())],
                    vec![GridCell::Integer(8), GridCell::Text("b".into())],
                ],
                columns(&int_text_kinds),
            ),
        ),
    ));
    out.push((
        "row header",
        provider(DatabaseType::SqlServer, &int_text).menu_items(&request(
            ContextMenuTarget::RowHeader {
                display_row_index: 0,
                source_row_index: 0,
            },
            None,
            &int_text_kinds,
            row(),
        )),
    ));
    out.push((
        "cell / redis key listing",
        provider(
            DatabaseType::Redis,
            &[
                ("key", "string"),
                ("type", "string"),
                ("ttl_seconds", "integer"),
            ],
        )
        .menu_items(&request(
            cell_target(0),
            None,
            &[
                ("key", ColumnKind::Text),
                ("type", ColumnKind::Text),
                ("ttl_seconds", ColumnKind::Integer),
            ],
            vec![
                GridCell::Text("user:1".into()),
                GridCell::Text("hash".into()),
                GridCell::Integer(60),
            ],
        )),
    ));
    out.push((
        "pivot / cell",
        ResultGridMenuProvider::new_pivot(WeakEntity::new_invalid(), None, 0)
            .with_export_prefs(ExportPreferences::default())
            .menu_items(&request(cell_target(0), None, &int_text_kinds, row())),
    ));
    out
}

/// Every choosable action in `items`, depth first, as `id :: label`.
pub(super) fn flatten(items: &[ContextMenuItem], out: &mut Vec<String>) {
    for item in items {
        match item {
            ContextMenuItem::Action { id, label } => out.push(format!("{id} :: {label}")),
            ContextMenuItem::BuiltIn(action) => out.push(format!("builtin :: {action:?}")),
            ContextMenuItem::Separator => {}
            ContextMenuItem::Submenu { items, .. } => flatten(items, out),
        }
    }
}

/// The before-and-after contract of any reorganisation: the set of actions
/// each grid context offers (with their labels) never changes. Sorted, so a
/// reorder alone passes; an added or dropped action fails against the
/// fixture.
#[test]
fn grid_menus_keep_every_action() {
    let mut text = String::from(
        "# Every action reachable from each results-grid context menu, sorted.\n\
         # Generated by sqlly-gpui result_grid_menu::layout_tests (SQLLY_UPDATE_GOLDEN=1 to refresh).\n",
    );
    for (name, items) in scenarios() {
        let mut lines = Vec::new();
        flatten(&items, &mut lines);
        lines.sort();
        text.push_str(&format!("### {name}\n"));
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grid-menus/actions.txt");
    assert!(
        sqlly_testkit::golden::GoldenTest::compare(&text, &fixture).expect("fixture"),
        "a results-grid menu gained or lost an action; if that is intended, refresh with SQLLY_UPDATE_GOLDEN=1 cargo test -p sqlly-gpui --lib grid_menus_keep_every_action"
    );
}

/// Top-level entries and separator-delimited groups of one menu level.
fn shape(items: &[ContextMenuItem]) -> (usize, usize) {
    let entries = items
        .iter()
        .filter(|i| !matches!(i, ContextMenuItem::Separator))
        .count();
    let groups = 1 + items
        .iter()
        .filter(|i| matches!(i, ContextMenuItem::Separator))
        .count();
    (entries, groups)
}

/// The everyday actions stay at the top level and the long tail lives in
/// submenus, so no grid menu outgrows a glance.
#[test]
fn grid_menus_stay_short() {
    let mut report = Vec::new();
    let mut too_big = false;
    for (name, items) in scenarios() {
        let (entries, groups) = shape(&items);
        report.push(format!("{name}: {entries} entries / {groups} groups"));
        too_big |= entries > 12 || groups > 4;
    }
    if std::env::var_os("SQLLY_MENU_REPORT").is_some() {
        eprintln!("{}", report.join("\n"));
    }
    assert!(
        !too_big,
        "a grid menu has more than 12 top-level entries or 4 groups:\n{}",
        report.join("\n")
    );
}

fn assert_no_empty_submenus(name: &str, items: &[ContextMenuItem]) {
    for item in items {
        if let ContextMenuItem::Submenu { label, items } = item {
            assert!(
                items
                    .iter()
                    .any(|i| !matches!(i, ContextMenuItem::Separator)),
                "{name}: submenu {label} is empty"
            );
            assert!(
                !matches!(items.first(), Some(ContextMenuItem::Separator))
                    && !matches!(items.last(), Some(ContextMenuItem::Separator))
                    && !items.windows(2).any(|w| {
                        matches!(w[0], ContextMenuItem::Separator)
                            && matches!(w[1], ContextMenuItem::Separator)
                    }),
                "{name}: submenu {label} has stray separators"
            );
            assert_no_empty_submenus(name, items);
        }
    }
}

/// A submenu whose items were all gated away is dropped, never shown empty.
#[test]
fn grid_menus_never_show_an_empty_submenu() {
    for (name, items) in scenarios() {
        assert_no_empty_submenus(name, &items);
    }
}

fn submenu<'a>(items: &'a [ContextMenuItem], label: &str) -> &'a [ContextMenuItem] {
    items
        .iter()
        .find_map(|i| match i {
            ContextMenuItem::Submenu { label: l, items } if l == label => Some(items.as_slice()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {label} submenu in {items:?}"))
}

fn ids(items: &[ContextMenuItem]) -> Vec<String> {
    let mut out = Vec::new();
    flatten(items, &mut out);
    out.into_iter()
        .map(|line| line.split(" :: ").next().unwrap_or_default().to_string())
        .collect()
}

/// The WHERE items moved into Filter keep the dry-run gate: a binary value
/// whose bytes the menu can't compare as text gets no text patterns there,
/// and the most common comparison sits on the top level, once.
#[test]
fn filter_submenu_keeps_the_where_dry_run() {
    let items = scenarios()
        .into_iter()
        .find(|(name, _)| *name == "cell / binary")
        .map(|(_, items)| items)
        .expect("scenario");
    let filter = ids(submenu(&items, "Filter"));
    assert!(
        filter.iter().any(|id| id == action_id::WHERE_IS_NULL),
        "{filter:?}"
    );
    for pattern in [
        action_id::WHERE_LIKE,
        action_id::WHERE_STARTS_WITH,
        action_id::WHERE_ENDS_WITH,
    ] {
        assert!(
            !filter.iter().any(|id| id == pattern),
            "{pattern} offered: {filter:?}"
        );
    }

    let text = scenarios()
        .into_iter()
        .find(|(name, _)| *name == "cell / text")
        .map(|(_, items)| items)
        .expect("scenario");
    let top: Vec<&str> = text
        .iter()
        .filter_map(|i| match i {
            ContextMenuItem::Action { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert!(top.contains(&action_id::WHERE_EQUALS), "{top:?}");
    let all = ids(&text);
    assert_eq!(
        all.iter()
            .filter(|id| *id == action_id::WHERE_EQUALS)
            .count(),
        1,
        "promoted, not duplicated"
    );
    assert!(
        ids(submenu(&text, "Filter"))
            .iter()
            .any(|id| id == action_id::WHERE_LIKE)
    );
}

const BINARY_ONLY: [&str; 3] = [
    action_id::VIEW_AS_HEX,
    action_id::CELL_SAVE_FILE,
    action_id::CELL_LOAD_FILE,
];

fn cell_menu_ids(db: DatabaseType, data_type: &str, kind: ColumnKind, value: &str) -> Vec<String> {
    ids(&provider(db, &[("c", data_type)]).menu_items(&request(
        cell_target(0),
        None,
        &[("c", kind)],
        vec![GridCell::Text(value.into())],
    )))
}

/// Whether a cell is binary comes from its column's type, not from text
/// that happens to start with `0x`: a wallet address in a text column gets
/// the text menu (transforms, the plain value viewer), never View as Hex or
/// the binary file actions.
#[test]
fn hex_looking_text_in_a_text_column_gets_the_text_menu() {
    for (db, data_type) in [
        (DatabaseType::Postgres, "text"),
        (DatabaseType::SqlServer, "nvarchar(42)"),
        (DatabaseType::MySql, "varchar(64)"),
        (DatabaseType::Sqlite, "TEXT"),
    ] {
        for value in [WALLET, "0xdeadbeef", "deadbeefcafe"] {
            let ids = cell_menu_ids(db, data_type, ColumnKind::Text, value);
            for binary in BINARY_ONLY {
                assert!(
                    !ids.iter().any(|id| id == binary),
                    "{db:?} {data_type} {value}: {binary} offered: {ids:?}"
                );
            }
            assert!(
                ids.iter().any(|id| id == "cell_transform_upper"),
                "{db:?} {data_type} {value}: no transforms: {ids:?}"
            );
            assert!(
                ids.iter().any(|id| id == action_id::STRUCTURED_DATA_PRETTY),
                "{db:?} {data_type} {value}: no value viewer: {ids:?}"
            );
            assert!(
                ids.iter().any(|id| id == action_id::QUICK_FILTER_EQ),
                "{db:?} {data_type} {value}: filters as text: {ids:?}"
            );
        }
    }
}

/// A genuine binary column keeps the binary actions, whether the grid
/// shows its bytes as hex (a `binary=hex` directive) or as the `<N bytes>`
/// stand-in, and its bytes are never offered text transforms.
#[test]
fn binary_columns_keep_the_binary_menu() {
    for (db, data_type) in [
        (DatabaseType::SqlServer, "varbinary(32)"),
        (DatabaseType::Postgres, "bytea"),
        (DatabaseType::MySql, "blob"),
        (DatabaseType::Sqlite, "BLOB"),
    ] {
        for value in ["0xDEADBEEF", "<16 bytes>"] {
            let ids = cell_menu_ids(db, data_type, ColumnKind::Text, value);
            for binary in BINARY_ONLY {
                assert!(
                    ids.iter().any(|id| id == binary),
                    "{db:?} {data_type} {value}: no {binary}: {ids:?}"
                );
            }
            assert!(
                !ids.iter().any(|id| id.starts_with("cell_transform_")),
                "{db:?} {data_type} {value}: transforms on bytes: {ids:?}"
            );
        }
    }
    // Bytes under a column the engine does not call binary (SQLite's
    // dynamic typing) show as the stand-in, which still reads as bytes.
    let ids = cell_menu_ids(DatabaseType::Sqlite, "TEXT", ColumnKind::Text, "<16 bytes>");
    for binary in BINARY_ONLY {
        assert!(ids.iter().any(|id| id == binary), "{binary}: {ids:?}");
    }
}

/// Widen / Narrow Column live in the column header's Column submenu, right
/// after the grid's Move Column Left/Right, and nowhere a menu has no
/// column to resize (the row header) or in Tools ▸ Layout.
#[test]
fn widen_and_narrow_sit_beside_move_column() {
    let all = scenarios();
    let find = |name: &str| {
        all.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, items)| items.as_slice())
            .expect("scenario")
    };
    let header = find("column header / int");
    let column = submenu(header, "Column");
    let at = column
        .iter()
        .position(|i| {
            matches!(
                i,
                ContextMenuItem::BuiltIn(sqlly_datatable::MenuAction::MoveColumnRight)
            )
        })
        .expect("Move Column Right");
    let next: Vec<&str> = column[at + 1..]
        .iter()
        .take(2)
        .filter_map(|i| match i {
            ContextMenuItem::Action { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        next,
        [action_id::WIDEN_COLUMN, action_id::NARROW_COLUMN],
        "{column:?}"
    );
    let tools = submenu(header, "Tools");
    let layout = ids(submenu(tools, "Layout"));
    assert!(
        !layout
            .iter()
            .any(|id| id == action_id::WIDEN_COLUMN || id == action_id::NARROW_COLUMN),
        "{layout:?}"
    );
    for (name, items) in &all {
        if name.starts_with("column header") {
            continue;
        }
        let ids = ids(items);
        assert!(
            !ids.iter()
                .any(|id| id == action_id::WIDEN_COLUMN || id == action_id::NARROW_COLUMN),
            "{name}: {ids:?}"
        );
    }
}
