//! Guards for the explorer's context-menu layout: reorganising a menu
//! (grouping into submenus, reordering) must never drop or add an action, and
//! a database's menu always opens with New Query.

use super::menus::{CustomActionEntry, MenuAvailability, MenuEntry, menu_entries_for_context};
use super::model::{ConstraintKind, NodeKind};
use super::*;
use sqlly_lang::sqlmodel::DatabaseType;
use std::path::Path;

fn ctx(kind: NodeKind) -> ExplorerActionContext {
    let (schema, name, table) = match &kind {
        NodeKind::Table { schema, name }
        | NodeKind::View { schema, name }
        | NodeKind::Procedure { schema, name }
        | NodeKind::Function { schema, name }
        | NodeKind::Type { schema, name }
        | NodeKind::Sequence { schema, name }
        | NodeKind::Synonym { schema, name, .. }
        | NodeKind::Event { schema, name }
        | NodeKind::MaterializedView { schema, name } => {
            (Some(schema.clone()), Some(name.clone()), None)
        }
        NodeKind::Index {
            schema,
            table,
            name,
        }
        | NodeKind::Statistic {
            schema,
            table,
            name,
        }
        | NodeKind::Constraint {
            schema,
            table,
            name,
            ..
        } => (
            Some(schema.clone()),
            Some(name.clone()),
            Some(table.clone()),
        ),
        NodeKind::Trigger {
            schema,
            table,
            name,
        } => (Some(schema.clone()), Some(name.clone()), table.clone()),
        NodeKind::Extension { name, .. } => (None, Some(name.clone()), None),
        NodeKind::SchemaFolder { schema } => (Some(schema.clone()), None, None),
        _ => (None, None, None),
    };
    let server_id = match &kind {
        NodeKind::Server { id } => Some(*id),
        _ => None,
    };
    ExplorerActionContext {
        node_id: "test".into(),
        title: "test".into(),
        kind,
        server_id,
        database: None,
        schema,
        name,
        table,
        copy_name: None,
        copy_qualified: None,
        columns: Vec::new(),
        profile: None,
        position: gpui::point(gpui::px(0.0), gpui::px(0.0)),
    }
}

fn s(v: &str) -> String {
    v.to_string()
}

/// One node of every kind the explorer builds a menu for.
fn kinds() -> Vec<(&'static str, NodeKind)> {
    vec![
        (
            "server",
            NodeKind::Server {
                id: uuid::Uuid::nil(),
            },
        ),
        ("database", NodeKind::Database(s("Sales"))),
        ("schema", NodeKind::SchemaFolder { schema: s("dbo") }),
        (
            "table",
            NodeKind::Table {
                schema: s("dbo"),
                name: s("Orders"),
            },
        ),
        (
            "view",
            NodeKind::View {
                schema: s("dbo"),
                name: s("v"),
            },
        ),
        (
            "procedure",
            NodeKind::Procedure {
                schema: s("dbo"),
                name: s("p"),
            },
        ),
        (
            "function",
            NodeKind::Function {
                schema: s("dbo"),
                name: s("f"),
            },
        ),
        ("column", NodeKind::Column),
        (
            "parameter",
            NodeKind::Parameter {
                data_type: s("int"),
                is_output: false,
            },
        ),
        (
            "index",
            NodeKind::Index {
                schema: s("dbo"),
                table: s("Orders"),
                name: s("ix"),
            },
        ),
        (
            "statistic",
            NodeKind::Statistic {
                schema: s("dbo"),
                table: s("Orders"),
                name: s("st"),
            },
        ),
        ("foreign key", NodeKind::ForeignKey { outgoing: true }),
        (
            "type",
            NodeKind::Type {
                schema: s("dbo"),
                name: s("t"),
            },
        ),
        (
            "trigger",
            NodeKind::Trigger {
                schema: s("dbo"),
                table: Some(s("Orders")),
                name: s("tr"),
            },
        ),
        (
            "constraint",
            NodeKind::Constraint {
                schema: s("dbo"),
                table: s("Orders"),
                name: s("ck"),
                kind: ConstraintKind::Check,
            },
        ),
        (
            "sequence",
            NodeKind::Sequence {
                schema: s("dbo"),
                name: s("seq"),
            },
        ),
        (
            "synonym",
            NodeKind::Synonym {
                schema: s("dbo"),
                name: s("syn"),
                base_object: s("dbo.Orders"),
            },
        ),
        (
            "extension",
            NodeKind::Extension {
                name: s("pgcrypto"),
                version: s("1.3"),
            },
        ),
        (
            "event",
            NodeKind::Event {
                schema: s("app"),
                name: s("ev"),
            },
        ),
        (
            "materialized view",
            NodeKind::MaterializedView {
                schema: s("public"),
                name: s("mv"),
            },
        ),
        ("tables folder", NodeKind::TablesFolder),
        ("views folder", NodeKind::ViewsFolder),
        ("programmability folder", NodeKind::ProgrammabilityFolder),
        (
            "indexes folder",
            NodeKind::IndexesFolder {
                schema: s("dbo"),
                table: s("Orders"),
            },
        ),
        ("extensions folder", NodeKind::ExtensionsFolder),
        ("sequences folder", NodeKind::SequencesFolder),
        ("error", NodeKind::Error),
        ("azure account", NodeKind::AzureAccount),
        (
            "pinned folder",
            NodeKind::PinnedFolder {
                server_id: uuid::Uuid::nil(),
                group: uuid::Uuid::nil(),
            },
        ),
        ("pinned", NodeKind::Pinned { index: 0 }),
        (
            "group",
            NodeKind::Group {
                color_hex: None,
                image_path: None,
            },
        ),
        (
            "redis key folder",
            NodeKind::RedisKeyFolder { prefix: s("user:") },
        ),
        (
            "redis key",
            NodeKind::RedisKey {
                key: s("user:1"),
                kind: s("hash"),
                ttl_seconds: None,
            },
        ),
        ("redis indexes folder", NodeKind::RedisIndexesFolder),
        ("redis index", NodeKind::RedisIndex { name: s("idx") }),
    ]
}

fn availability(db: DatabaseType) -> MenuAvailability {
    MenuAvailability {
        is_connected: true,
        is_loading: false,
        can_mutate: true,
        has_connection: true,
        db,
        custom_actions: vec![CustomActionEntry {
            name: s("Audit"),
            mode: sqlly_services::ExplorerActionMode::Script,
        }],
        connections_read_only: false,
        redis_scan_more: false,
        pin_groups: Vec::new(),
    }
}

fn offline(db: DatabaseType) -> MenuAvailability {
    MenuAvailability {
        is_connected: false,
        can_mutate: false,
        has_connection: false,
        custom_actions: Vec::new(),
        ..availability(db)
    }
}

const ENGINES: &[DatabaseType] = &[
    DatabaseType::SqlServer,
    DatabaseType::Postgres,
    DatabaseType::MySql,
    DatabaseType::Sqlite,
    DatabaseType::Oracle,
    DatabaseType::DuckDb,
    DatabaseType::ClickHouse,
    DatabaseType::Redis,
];

/// Every action reachable from `entries`, through any depth of submenus, with
/// its effective enabled state (an item inside a disabled submenu is shown
/// disabled, so moving it in or out of one is judged by what the user sees).
fn reachable(entries: &[MenuEntry], parent_enabled: bool, out: &mut Vec<String>) {
    for entry in entries {
        match entry {
            MenuEntry::Action {
                label,
                action,
                enabled,
            } => out.push(format!(
                "{label} :: {action:?} :: {}",
                parent_enabled && *enabled
            )),
            MenuEntry::ActionOwned {
                label,
                action,
                enabled,
            } => out.push(format!(
                "{label} :: {action:?} :: {}",
                parent_enabled && *enabled
            )),
            MenuEntry::SubmenuOwned {
                entries, enabled, ..
            } => reachable(entries, parent_enabled && *enabled, out),
            MenuEntry::Separator => {}
        }
    }
}

fn scenarios() -> Vec<(String, Vec<MenuEntry>)> {
    let mut out = Vec::new();
    for db in ENGINES {
        for (name, kind) in kinds() {
            for (state, avail) in [("online", availability(*db)), ("offline", offline(*db))] {
                let entries = menu_entries_for_context(&ctx(kind.clone()), &avail);
                out.push((format!("{db:?} / {name} / {state}"), entries));
            }
        }
    }
    out
}

/// Reorganising a menu may move, regroup or nest its items, but the set of
/// actions you can reach (with their labels and enabled states) is fixed by
/// this fixture.
#[test]
fn explorer_menus_keep_every_action() {
    let mut text = String::from(
        "# Every action reachable from each explorer context menu, sorted.\n\
         # Generated by sqlly-gpui schema_tree::menu_layout_tests (SQLLY_UPDATE_GOLDEN=1 to refresh).\n",
    );
    for (name, entries) in scenarios() {
        let mut lines = Vec::new();
        reachable(&entries, true, &mut lines);
        lines.sort();
        text.push_str(&format!("### {name}\n"));
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/explorer-menus/actions.txt");
    assert!(
        sqlly_testkit::golden::GoldenTest::compare(&text, &fixture).expect("fixture"),
        "an explorer menu gained or lost an action; if that is intended, refresh with SQLLY_UPDATE_GOLDEN=1 cargo test -p sqlly-gpui --lib explorer_menus_keep_every_action"
    );
}

/// Indented outline of a menu, for reviewing a layout change by eye
/// (`cargo test -p sqlly-gpui --lib print_explorer_menu_outlines -- --ignored --nocapture`).
fn outline(entries: &[MenuEntry], depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    for entry in entries {
        match entry {
            MenuEntry::Action { label, .. } => out.push_str(&format!("{pad}{label}\n")),
            MenuEntry::ActionOwned { label, .. } => out.push_str(&format!("{pad}{label}\n")),
            MenuEntry::SubmenuOwned { label, entries, .. } => {
                out.push_str(&format!("{pad}{label} ▸\n"));
                outline(entries, depth + 1, out);
            }
            MenuEntry::Separator => out.push_str(&format!("{pad}---\n")),
        }
    }
}

#[test]
#[ignore]
fn print_explorer_menu_outlines() {
    for (name, entries) in scenarios() {
        if !name.starts_with("SqlServer /") && !name.starts_with("Redis /") {
            continue;
        }
        if name.ends_with("offline") {
            continue;
        }
        let top = entries
            .iter()
            .filter(|e| !matches!(e, MenuEntry::Separator))
            .count();
        let groups = entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::Separator))
            .count()
            + 1;
        let mut text = String::new();
        outline(&entries, 1, &mut text);
        println!("== {name} ({top} top-level, {groups} groups)\n{text}");
    }
}

/// Right-clicking a database always offers New Query first, on every engine
/// and whatever state the connection is in.
#[test]
fn database_menu_opens_with_new_query() {
    for db in ENGINES {
        let states = [
            ("online", availability(*db)),
            ("offline", offline(*db)),
            (
                "loading",
                MenuAvailability {
                    is_loading: true,
                    ..availability(*db)
                },
            ),
            (
                "read-only connections",
                MenuAvailability {
                    connections_read_only: true,
                    can_mutate: false,
                    ..availability(*db)
                },
            ),
            (
                "scan pending",
                MenuAvailability {
                    redis_scan_more: true,
                    ..availability(*db)
                },
            ),
        ];
        for (state, avail) in states {
            for name in ["Sales", "master", "tempdb", "0"] {
                let entries =
                    menu_entries_for_context(&ctx(NodeKind::Database(name.into())), &avail);
                match entries.first() {
                    Some(MenuEntry::ActionOwned {
                        label,
                        action: super::menus::ExplorerMenuAction::NewQuery,
                        ..
                    }) if label == "New Query" => {}
                    other => panic!("{db:?} / {name} / {state}: first entry is {other:?}"),
                }
            }
        }
    }
}

/// The large menus stay organised: at most 4 separator-delimited groups, no
/// empty group, and a bounded top level. Tables carry the most actions; their
/// menu keeps its frequent ones (Select, View, Filter, Count, Design, Edit
/// Structure, Rename) visible, at 17 top-level entries. Every other menu stays
/// at 13 or fewer.
#[test]
fn large_menus_stay_grouped() {
    for (name, entries) in scenarios() {
        let top = entries
            .iter()
            .filter(|e| !matches!(e, MenuEntry::Separator))
            .count();
        let groups = entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::Separator))
            .count()
            + 1;
        let limit = if name.contains(" / table / ") { 17 } else { 13 };
        assert!(top <= limit, "{name}: {top} top-level entries");
        assert!(groups <= 4, "{name}: {groups} groups");
        assert!(
            !matches!(entries.first(), Some(MenuEntry::Separator))
                && !matches!(entries.last(), Some(MenuEntry::Separator))
                && !entries
                    .windows(2)
                    .any(|w| matches!(w, [MenuEntry::Separator, MenuEntry::Separator])),
            "{name}: stray separator"
        );
    }
}

/// The database menu sorts its tool windows into Data, Schema and Tools; each
/// one in `DATABASE_TOOLS` is still offered exactly once.
#[test]
fn database_menu_offers_every_database_tool_once() {
    use super::menus::{DATABASE_TOOLS, ExplorerMenuAction};
    fn tools(entries: &[MenuEntry], out: &mut Vec<String>) {
        for entry in entries {
            match entry {
                MenuEntry::Action {
                    action: ExplorerMenuAction::Tool(t),
                    ..
                }
                | MenuEntry::ActionOwned {
                    action: ExplorerMenuAction::Tool(t),
                    ..
                } => out.push(format!("{t:?}")),
                MenuEntry::SubmenuOwned { entries, .. } => tools(entries, out),
                _ => {}
            }
        }
    }
    let entries = menu_entries_for_context(
        &ctx(NodeKind::Database("Sales".into())),
        &availability(DatabaseType::SqlServer),
    );
    let mut found = Vec::new();
    tools(&entries, &mut found);
    found.sort();
    let mut expected: Vec<String> = DATABASE_TOOLS.iter().map(|t| format!("{t:?}")).collect();
    expected.sort();
    assert_eq!(found, expected);
}

fn has_action(entries: &[MenuEntry], want: &super::ExplorerMenuAction) -> bool {
    entries.iter().any(|entry| match entry {
        MenuEntry::Action { action, .. } | MenuEntry::ActionOwned { action, .. } => action == want,
        MenuEntry::SubmenuOwned { entries, .. } => has_action(entries, want),
        MenuEntry::Separator => false,
    })
}

/// Every schema folder offers Hide Schema.
#[test]
fn schema_folder_menu_offers_hide_schema() {
    let entries = menu_entries_for_context(
        &ctx(NodeKind::SchemaFolder {
            schema: s("staging"),
        }),
        &availability(DatabaseType::Postgres),
    );
    assert!(has_action(&entries, &super::ExplorerMenuAction::HideSchema));
}

/// A database offers Show Hidden Schemas (with the count) only when its
/// connection hides schemas in that database.
#[test]
fn database_menu_offers_show_hidden_schemas_only_when_some_are_hidden() {
    use sqlly_core::{SecretProvider, SecretRef};
    let mut profile = sqlly_core::ConnectionProfile::sql_login(
        "p",
        "h",
        "u",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: s("k"),
        },
    );
    let menu = |profile: &sqlly_core::ConnectionProfile, db: &str| {
        let mut c = ctx(NodeKind::Database(s(db)));
        c.profile = Some(Box::new(profile.clone()));
        menu_entries_for_context(&c, &availability(DatabaseType::SqlServer))
    };
    let show = super::ExplorerMenuAction::ShowHiddenSchemas;
    assert!(!has_action(&menu(&profile, "Sales"), &show));
    profile.hide_schema("Sales", "staging");
    profile.hide_schema("Sales", "audit");
    let sales = menu(&profile, "Sales");
    assert!(has_action(&sales, &show));
    let label = sales.iter().find_map(|e| match e {
        MenuEntry::ActionOwned { label, action, .. } if *action == show => Some(label.clone()),
        _ => None,
    });
    assert!(label.is_some_and(|l| l.contains('2')), "count in the label");
    // Hidden schemas are per database.
    assert!(!has_action(&menu(&profile, "Hr"), &show));
}

/// In the flat `schema.name` layout there is no schema node, so a
/// schema-qualified object offers Hide Schema "<schema>" itself; in the
/// schema-folders layout (a `:sch:` id) and on engines without schemas it
/// does not.
#[test]
fn flat_layout_objects_offer_hide_their_schema() {
    let hide = super::ExplorerMenuAction::HideSchema;
    let table = NodeKind::Table {
        schema: s("staging"),
        name: s("Orders"),
    };
    let mut flat = ctx(table.clone());
    flat.node_id = s("srv:x:db:Sales:tables:t:staging:Orders");
    let entries = menu_entries_for_context(&flat, &availability(DatabaseType::SqlServer));
    // Inside a submenu (Tools), so the table menu's top level does not grow.
    assert!(
        !entries.iter().any(|e| matches!(
            e,
            MenuEntry::ActionOwned { action, .. } if *action == hide
        )),
        "not at the top level of the table menu"
    );
    let label = entries.iter().find_map(|e| match e {
        MenuEntry::SubmenuOwned { entries, .. } => entries.iter().find_map(|e| match e {
            MenuEntry::ActionOwned { label, action, .. } if *action == hide => Some(label.clone()),
            _ => None,
        }),
        _ => None,
    });
    assert!(
        label.is_some_and(|l| l.contains("staging")),
        "flat table names its schema"
    );
    // A leaf object without submenus still offers it.
    let mut sequence = ctx(NodeKind::Sequence {
        schema: s("staging"),
        name: s("seq"),
    });
    sequence.node_id = s("srv:x:db:Sales:sequences:seq:staging:seq");
    assert!(has_action(
        &menu_entries_for_context(&sequence, &availability(DatabaseType::Postgres)),
        &hide
    ));
    let view = NodeKind::View {
        schema: s("staging"),
        name: s("V"),
    };
    let mut flat_view = ctx(view);
    flat_view.node_id = s("srv:x:db:Sales:views:v:staging:V");
    assert!(has_action(
        &menu_entries_for_context(&flat_view, &availability(DatabaseType::Postgres)),
        &hide
    ));

    let mut scoped = ctx(table.clone());
    scoped.node_id = s("srv:x:db:Sales:sch:staging:tables:t:staging:Orders");
    assert!(!has_action(
        &menu_entries_for_context(&scoped, &availability(DatabaseType::SqlServer)),
        &hide
    ));
    let sqlite = ctx(NodeKind::Table {
        schema: s(""),
        name: s("t"),
    });
    assert!(!has_action(
        &menu_entries_for_context(&sqlite, &availability(DatabaseType::Sqlite)),
        &hide
    ));
}
