//! Native, cross-connection Schema Compare. Source and target routes are
//! captured independently, metadata is normalized only where the engines
//! expose comparable facts, and generated migration text is review-only.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, ResultColumn};
use sqlly_datatable::{CellValue as GridCell, Column, ColumnKind, GridData, SqllyDataTable};
use sqlly_lang::sqlmodel::ConnectionKind;
use sqlly_services::{
    ComparedDifference, DeployOptions, DifferenceStatus, RenameCandidate, SchemaObject,
    apply_renames, compare_objects, deploy_script, detect_renames, plan_deploy, rollback_script,
};

use crate::app::StatusLevel;
use crate::connection::ConnectionStore;
use crate::dialog::{Dialog, DialogFrame};
use crate::schema_compare_risk::{Risk, base_risk};
use crate::schema_snapshots::{CompareSource, SchemaSnapshotMeta, picker_label, snapshot_title};
use crate::service_router::ServiceRouter;
use crate::theme::Theme;
use crate::tool_dialog::{
    TOOL_QUERY_ROW_CAP, ToolDialogContext, ToolDialogEvent, ToolQueryOutcome, ToolQuerySet,
};

const MAX_COMPARE_PROFILES: usize = 200;

#[cfg(not(target_arch = "wasm32"))]
use crate::history_panel::{DiffKind, diff_lines_capped};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CompareSide {
    Source,
    Target,
}

/// Saved schema snapshots offered in the side pickers. The on-disk store is
/// native-only; the browser preview simply has none.
#[cfg(not(target_arch = "wasm32"))]
fn list_snapshots() -> Vec<SchemaSnapshotMeta> {
    crate::schema_snapshots::SchemaSnapshotStore::default_store().list_all()
}

#[cfg(target_arch = "wasm32")]
fn list_snapshots() -> Vec<SchemaSnapshotMeta> {
    Vec::new()
}

#[cfg(not(target_arch = "wasm32"))]
fn load_snapshot_objects(
    meta: &SchemaSnapshotMeta,
) -> Result<Vec<SchemaObject>, sqlly_core::DisplayMessage> {
    crate::schema_snapshots::SchemaSnapshotStore::default_store().read(&meta.id)
}

#[cfg(target_arch = "wasm32")]
fn load_snapshot_objects(
    _meta: &SchemaSnapshotMeta,
) -> Result<Vec<SchemaObject>, sqlly_core::DisplayMessage> {
    let message = crate::localization::text("explorer.schema_compare.snapshots_unavailable");
    Err(crate::browser_preview::unavailable(&message).into())
}

/// What one side of the comparison reads: a saved snapshot (read from disk
/// off the UI thread) or a metadata batch to run against a live database.
enum SideInput {
    Snapshot(SchemaSnapshotMeta),
    Query {
        context: Box<ToolDialogContext>,
        sql: String,
    },
}

/// Object-collection batch for one database: set 0 = module-backed objects
/// with definitions, set 1 = tables with a column-signature
/// pseudo-definition, set 2 = indexes, set 3 = foreign keys, set 4 = check
/// and default constraints. The index/FK/constraint "definitions" are
/// comment-shaped signatures — informative in the diff and inert in the
/// generated script, since they are descriptions rather than runnable DDL.
fn collect_objects_sql_tds(database: &str) -> String {
    let db = database.replace(']', "]]");
    format!(
        "USE [{db}];\n\
         SELECT s.name AS schema_name, \
         CASE WHEN o.type = 'TR' THEN CONCAT(OBJECT_NAME(o.parent_object_id), '.', o.name) ELSE o.name END AS object_name, \
         CASE o.type WHEN 'V' THEN 'VIEW' WHEN 'TR' THEN 'TRIGGER' ELSE 'ROUTINE' END AS kind, \
         m.definition \
         FROM sys.objects o \
         JOIN sys.schemas s ON o.schema_id = s.schema_id \
         JOIN sys.sql_modules m ON o.object_id = m.object_id \
         ORDER BY s.name, o.name;\n\
         SELECT s.name AS schema_name, t.name AS object_name, 'TABLE' AS kind, \
         ISNULL((SELECT STRING_AGG(CONCAT(c.name, ' ', ty.name, '(', c.max_length, ') ', \
         CASE c.is_nullable WHEN 1 THEN 'NULL' ELSE 'NOT NULL' END), ', ') \
         WITHIN GROUP (ORDER BY c.column_id) \
         FROM sys.columns c JOIN sys.types ty ON c.user_type_id = ty.user_type_id \
         WHERE c.object_id = t.object_id), '') AS definition \
         FROM sys.tables t \
         JOIN sys.schemas s ON t.schema_id = s.schema_id \
         ORDER BY s.name, t.name;\n\
         SELECT s.name AS schema_name, CONCAT(t.name, '.', i.name) AS object_name, \
         'INDEX' AS kind, \
         CONCAT('-- ', CASE i.is_unique WHEN 1 THEN 'unique ' ELSE '' END, \
         LOWER(i.type_desc COLLATE DATABASE_DEFAULT), ' (', \
         ISNULL((SELECT STRING_AGG(c.name, ', ') WITHIN GROUP (ORDER BY ic.key_ordinal) \
         FROM sys.index_columns ic \
         JOIN sys.columns c ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id \
         AND ic.is_included_column = 0), ''), ')', \
         ISNULL((SELECT CONCAT(' include (', STRING_AGG(c.name, ', '), ')') \
         FROM sys.index_columns ic \
         JOIN sys.columns c ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id \
         AND ic.is_included_column = 1), '')) AS definition \
         FROM sys.indexes i \
         JOIN sys.tables t ON i.object_id = t.object_id \
         JOIN sys.schemas s ON t.schema_id = s.schema_id \
         WHERE i.name IS NOT NULL \
         ORDER BY s.name, t.name, i.name;\n\
         SELECT s.name AS schema_name, CONCAT(t.name, '.', fk.name) AS object_name, \
         'FOREIGN KEY' AS kind, \
         CONCAT('-- (', \
         ISNULL((SELECT STRING_AGG(pc.name, ', ') \
         FROM sys.foreign_key_columns fkc \
         JOIN sys.columns pc ON fkc.parent_object_id = pc.object_id \
         AND fkc.parent_column_id = pc.column_id \
         WHERE fkc.constraint_object_id = fk.object_id), ''), \
         ') references ', rs.name, '.', rt.name, ' (', \
         ISNULL((SELECT STRING_AGG(rc.name, ', ') \
         FROM sys.foreign_key_columns fkc \
         JOIN sys.columns rc ON fkc.referenced_object_id = rc.object_id \
         AND fkc.referenced_column_id = rc.column_id \
         WHERE fkc.constraint_object_id = fk.object_id), ''), \
         ') on delete ', LOWER(fk.delete_referential_action_desc COLLATE DATABASE_DEFAULT), \
         ' on update ', LOWER(fk.update_referential_action_desc COLLATE DATABASE_DEFAULT)) AS definition \
         FROM sys.foreign_keys fk \
         JOIN sys.tables t ON fk.parent_object_id = t.object_id \
         JOIN sys.schemas s ON t.schema_id = s.schema_id \
         JOIN sys.tables rt ON fk.referenced_object_id = rt.object_id \
         JOIN sys.schemas rs ON rt.schema_id = rs.schema_id \
         ORDER BY s.name, t.name, fk.name;\n\
         SELECT s.name AS schema_name, CONCAT(t.name, '.', cc.name) AS object_name, \
         'CHECK CONSTRAINT' AS kind, CONCAT('-- ', cc.definition) AS definition \
         FROM sys.check_constraints cc \
         JOIN sys.tables t ON cc.parent_object_id = t.object_id \
         JOIN sys.schemas s ON t.schema_id = s.schema_id \
         UNION ALL \
         SELECT s.name, CONCAT(t.name, '.', dc.name), 'DEFAULT CONSTRAINT', \
         CONCAT('-- ', c.name, ' default ', dc.definition) \
         FROM sys.default_constraints dc \
         JOIN sys.tables t ON dc.parent_object_id = t.object_id \
         JOIN sys.schemas s ON t.schema_id = s.schema_id \
         JOIN sys.columns c ON dc.parent_object_id = c.object_id \
         AND dc.parent_column_id = c.column_id \
         ORDER BY schema_name, object_name;\n\
         SELECT s.name, CONCAT(t.name, '.', kc.name), \
         CASE kc.type WHEN 'PK' THEN 'PRIMARY KEY' ELSE 'UNIQUE CONSTRAINT' END, \
         ISNULL((SELECT STRING_AGG(c.name, ', ') WITHIN GROUP (ORDER BY ic.key_ordinal) \
         FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id), '') \
         FROM sys.key_constraints kc JOIN sys.tables t ON t.object_id = kc.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = t.schema_id \
         ORDER BY s.name, t.name, kc.name;"
    )
}

/// A ClickHouse string literal: backslash is an escape character there, so
/// it is doubled along with the quote.
fn clickhouse_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "''")
}

/// ClickHouse: one database's objects from its system tables. Tables get a
/// column signature (name, type, default kind and expression, codec) plus
/// the engine clause (`MergeTree PARTITION BY … ORDER BY …`); views,
/// materialized views and dictionaries their server-rendered CREATE
/// statement; data-skipping indexes, primary keys and CHECK constraints
/// their definitions.
fn collect_objects_sql_clickhouse(database: &str) -> String {
    let db = clickhouse_literal(database);
    format!(
        "SELECT t.database, t.name, 'TABLE', concat(c.columns, ' ENGINE = ', t.engine_full) FROM system.tables AS t JOIN (SELECT database, table, arrayStringConcat(arrayMap(x -> x.2, arraySort(x -> x.1, groupArray((position, concat(name, ' ', type, if(default_kind != '', concat(' ', default_kind, ' ', default_expression), ''), if(compression_codec != '', concat(' ', compression_codec), '')))))), ', ') AS columns FROM system.columns WHERE database = '{db}' GROUP BY database, table) AS c ON c.database = t.database AND c.table = t.name WHERE t.database = '{db}' AND t.engine NOT IN ('View', 'LiveView', 'WindowView', 'MaterializedView', 'Dictionary') AND t.name NOT LIKE '.inner%' ORDER BY t.database, t.name;\n\
         SELECT database, name, multiIf(engine = 'MaterializedView', 'MATERIALIZED VIEW', engine = 'Dictionary', 'DICTIONARY', 'VIEW'), create_table_query FROM system.tables WHERE database = '{db}' AND engine IN ('View', 'LiveView', 'WindowView', 'MaterializedView', 'Dictionary') ORDER BY database, name;\n\
         SELECT database, concat(table, '.', name), 'INDEX', concat(type_full, ' ON (', expr, ') GRANULARITY ', toString(granularity)) FROM system.data_skipping_indices WHERE database = '{db}' ORDER BY database, table, name;\n\
         SELECT database, concat(name, '.PRIMARY'), 'PRIMARY KEY', primary_key FROM system.tables WHERE database = '{db}' AND primary_key != '' AND name NOT LIKE '.inner%' ORDER BY database, name;\n\
         SELECT database, concat(table, '.', name), 'CHECK CONSTRAINT', concat(type, ' ', expression) FROM system.constraints WHERE database = '{db}' ORDER BY database, table, name;"
    )
}

/// One bounded, read-only metadata batch for a native engine. Every result
/// set has the same four text columns: schema, name, normalized kind, and a
/// definition/signature. Unsupported engine families are rejected before I/O.
pub fn collect_objects_sql(database_type: DatabaseType, database: &str) -> Result<String, String> {
    match database_type.connection_kind() {
        ConnectionKind::Tds => Ok(collect_objects_sql_tds(database)),
        ConnectionKind::ClickHouseHttp => Ok(collect_objects_sql_clickhouse(database)),
        ConnectionKind::PostgresWire => Ok(
            "SELECT schemaname, viewname, 'VIEW', 'CREATE VIEW ' || quote_ident(schemaname) || '.' || quote_ident(viewname) || ' AS ' || definition FROM pg_views WHERE schemaname NOT IN ('pg_catalog', 'information_schema');\n\
             SELECT c.table_schema, c.table_name, 'TABLE', string_agg(c.column_name || ' ' || c.data_type || CASE WHEN c.is_nullable = 'YES' THEN ' NULL' ELSE ' NOT NULL' END, ', ' ORDER BY c.ordinal_position) FROM information_schema.columns c JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') AND t.table_type = 'BASE TABLE' GROUP BY c.table_schema, c.table_name;\n\
             SELECT schemaname, tablename || '.' || indexname, 'INDEX', indexdef FROM pg_indexes WHERE schemaname NOT IN ('pg_catalog', 'information_schema');\n\
             SELECT n.nspname, c.relname || '.' || con.conname, CASE con.contype WHEN 'f' THEN 'FOREIGN KEY' WHEN 'c' THEN 'CHECK CONSTRAINT' WHEN 'p' THEN 'PRIMARY KEY' WHEN 'u' THEN 'UNIQUE CONSTRAINT' END, pg_get_constraintdef(con.oid, true) FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND con.contype IN ('f','c','p','u');\n\
             SELECT n.nspname, p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')', 'ROUTINE', pg_get_functiondef(p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname NOT IN ('pg_catalog', 'information_schema');\n\
             SELECT n.nspname, c.relname || '.' || t.tgname, 'TRIGGER', pg_get_triggerdef(t.oid, true) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE NOT t.tgisinternal AND n.nspname NOT IN ('pg_catalog', 'information_schema');"
                .to_string(),
        ),
        ConnectionKind::MySqlWire => Ok(
            "SELECT table_schema, table_name, 'VIEW', CONCAT('CREATE VIEW `', REPLACE(table_schema, '`', '``'), '`.`', REPLACE(table_name, '`', '``'), '` AS ', view_definition) FROM information_schema.views WHERE table_schema = DATABASE();\n\
             SELECT c.table_schema, c.table_name, 'TABLE', GROUP_CONCAT(CONCAT(c.column_name, ' ', c.column_type, IF(c.is_nullable = 'YES', ' NULL', ' NOT NULL'), IF(c.column_default IS NULL, '', CONCAT(' DEFAULT ', c.column_default))) ORDER BY c.ordinal_position SEPARATOR ', ') FROM information_schema.columns c JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name WHERE c.table_schema = DATABASE() AND t.table_type = 'BASE TABLE' GROUP BY c.table_schema, c.table_name;\n\
             SELECT table_schema, CONCAT(table_name, '.', index_name), 'INDEX', GROUP_CONCAT(CONCAT(column_name, IF(collation = 'D', ' DESC', '')) ORDER BY seq_in_index SEPARATOR ', ') FROM information_schema.statistics WHERE table_schema = DATABASE() GROUP BY table_schema, table_name, index_name;\n\
             SELECT k.constraint_schema, CONCAT(k.table_name, '.', k.constraint_name), 'FOREIGN KEY', CONCAT(GROUP_CONCAT(k.column_name ORDER BY k.ordinal_position), ' REFERENCES ', MAX(k.referenced_table_schema), '.', MAX(k.referenced_table_name), ' (', GROUP_CONCAT(k.referenced_column_name ORDER BY k.position_in_unique_constraint), ')') FROM information_schema.key_column_usage k WHERE k.table_schema = DATABASE() AND k.referenced_table_name IS NOT NULL GROUP BY k.constraint_schema, k.table_name, k.constraint_name;\n\
             SELECT tc.constraint_schema, CONCAT(tc.table_name, '.', tc.constraint_name), CASE tc.constraint_type WHEN 'PRIMARY KEY' THEN 'PRIMARY KEY' ELSE 'UNIQUE CONSTRAINT' END, GROUP_CONCAT(k.column_name ORDER BY k.ordinal_position) FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage k ON k.constraint_schema = tc.constraint_schema AND k.table_name = tc.table_name AND k.constraint_name = tc.constraint_name WHERE tc.table_schema = DATABASE() AND tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE') GROUP BY tc.constraint_schema, tc.table_name, tc.constraint_name, tc.constraint_type;\n\
             SELECT tc.constraint_schema, CONCAT(tc.table_name, '.', tc.constraint_name), 'CHECK CONSTRAINT', cc.check_clause FROM information_schema.table_constraints tc JOIN information_schema.check_constraints cc ON cc.constraint_schema = tc.constraint_schema AND cc.constraint_name = tc.constraint_name WHERE tc.table_schema = DATABASE() AND tc.constraint_type = 'CHECK';\n\
             SELECT routine_schema, routine_name, 'ROUTINE', routine_definition FROM information_schema.routines WHERE routine_schema = DATABASE();\n\
             SELECT trigger_schema, CONCAT(event_object_table, '.', trigger_name), 'TRIGGER', CONCAT('CREATE TRIGGER ', trigger_name, ' ', action_timing, ' ', event_manipulation, ' ON ', event_object_table, ' FOR EACH ROW ', action_statement) FROM information_schema.triggers WHERE trigger_schema = DATABASE();"
                .to_string(),
        ),
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => Ok(
            "SELECT '', name, UPPER(type), COALESCE(sql, '') FROM sqlite_master WHERE type IN ('table','view','index','trigger') AND name NOT LIKE 'sqlite_%' ORDER BY type, name;\n\
             SELECT '', m.name || '.fk' || f.id, 'FOREIGN KEY', GROUP_CONCAT(f.\"from\" || ' REFERENCES ' || f.\"table\" || '(' || f.\"to\" || ')', ', ') FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) f WHERE m.type = 'table' GROUP BY m.name, f.id;"
                .to_string(),
        ),
        // DuckDB stores the CREATE source of tables, views, and indexes in
        // its catalog functions; constraints and sequences are rendered as
        // signatures (constraints as comments so scripts stay inert; a
        // sequence from its declared bounds rather than its current value,
        // which the stored `sql` folds in). Macros are DuckDB's routines.
        ConnectionKind::DuckDbFile => Ok(
            "SELECT schema_name, table_name, 'TABLE', COALESCE(sql, '') FROM duckdb_tables() WHERE database_name = current_database() AND NOT internal ORDER BY schema_name, table_name;\n\
             SELECT schema_name, view_name, 'VIEW', COALESCE(sql, '') FROM duckdb_views() WHERE database_name = current_database() AND NOT internal ORDER BY schema_name, view_name;\n\
             SELECT schema_name, table_name || '.' || index_name, 'INDEX', COALESCE(sql, '') FROM duckdb_indexes() WHERE database_name = current_database() ORDER BY schema_name, table_name, index_name;\n\
             SELECT schema_name, table_name || '.' || COALESCE(constraint_name, constraint_type || '_' || CAST(constraint_index AS VARCHAR)), CASE constraint_type WHEN 'FOREIGN KEY' THEN 'FOREIGN KEY' WHEN 'PRIMARY KEY' THEN 'PRIMARY KEY' WHEN 'UNIQUE' THEN 'UNIQUE CONSTRAINT' ELSE 'CHECK CONSTRAINT' END, '-- ' || constraint_text FROM duckdb_constraints() WHERE database_name = current_database() AND constraint_type IN ('FOREIGN KEY', 'PRIMARY KEY', 'UNIQUE', 'CHECK') ORDER BY schema_name, table_name, constraint_index;\n\
             SELECT schema_name, sequence_name, 'SEQUENCE', 'CREATE SEQUENCE ' || sequence_name || ' INCREMENT BY ' || increment_by || ' MINVALUE ' || min_value || ' MAXVALUE ' || max_value || ' START ' || start_value || CASE WHEN cycle THEN ' CYCLE' ELSE ' NO CYCLE' END || ';' FROM duckdb_sequences() WHERE database_name = current_database() AND NOT temporary ORDER BY schema_name, sequence_name;\n\
             SELECT schema_name, function_name, 'ROUTINE', 'CREATE MACRO ' || function_name || '(' || COALESCE(array_to_string(parameters, ', '), '') || ') AS ' || CASE function_type WHEN 'table_macro' THEN 'TABLE ' ELSE '' END || macro_definition || ';' FROM duckdb_functions() WHERE database_name = current_database() AND NOT internal AND function_type IN ('macro', 'table_macro') ORDER BY schema_name, function_name;"
                .to_string(),
        ),
        // Oracle: DBMS_METADATA DDL for the session's current schema (the
        // profile's service is the "database"; the schema is the login's).
        ConnectionKind::OracleNet => {
            Ok(sqlly_services::admin::schema_plan::oracle_collect_objects_sql())
        }
        _ => Err(
            crate::localization::format(
                "explorer.schema_compare.no_collector",
                &[("engine", &database_type.display_name())],
            ),
        ),
    }
}

fn database_list_sql(database_type: DatabaseType) -> Option<&'static str> {
    match database_type.connection_kind() {
        ConnectionKind::Tds => {
            Some("SELECT name FROM sys.databases WHERE state = 0 AND database_id > 4 ORDER BY name")
        }
        ConnectionKind::PostgresWire => Some(
            "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname",
        ),
        ConnectionKind::MySqlWire => Some(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN ('information_schema','mysql','performance_schema','sys') ORDER BY schema_name",
        ),
        ConnectionKind::ClickHouseHttp => Some(
            "SELECT name FROM system.databases WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name",
        ),
        ConnectionKind::SqliteFile => None,
        // A DuckDB profile is one database file; the collector reads the
        // catalog the connection opened.
        ConnectionKind::DuckDbFile => None,
        _ => None,
    }
}

/// Convert one collection outcome (both sets) into `SchemaObject`s.
pub fn objects_from_outcome(outcome: &ToolQueryOutcome) -> Vec<SchemaObject> {
    let mut objects = Vec::new();
    for set in &outcome.sets {
        for row in &set.rows {
            let text = |i: usize| match row.get(i) {
                Some(CellValue::String(s)) => s.clone(),
                Some(CellValue::Null) | None => String::new(),
                Some(other) => format!("{other:?}"),
            };
            objects.push(SchemaObject {
                schema: text(0),
                name: text(1),
                kind: text(2),
                definition: text(3),
            });
        }
    }
    objects
}

pub(crate) fn collection_limit_reached(outcome: &ToolQueryOutcome) -> bool {
    outcome
        .sets
        .iter()
        .any(|set| set.rows.len() >= TOOL_QUERY_ROW_CAP)
}

/// Diff rows shaped for the grid. `risks` is parallel to `differences` and
/// fills the trailing Risk column; both slices arrive already in display
/// order.
fn diff_grid_set(differences: &[ComparedDifference], risks: &[Risk]) -> ToolQuerySet {
    let columns = [
        "explorer.schema_compare.column_change",
        "explorer.schema_compare.column_kind",
        "explorer.schema_compare.column_object",
        "explorer.schema_compare.column_review_note",
        "explorer.schema_compare.column_risk",
    ]
    .into_iter()
    .map(|name| ResultColumn {
        name: crate::localization::text(name),
        data_type: "nvarchar".to_string(),
        nullable: false,
    })
    .collect();
    let rows = differences
        .iter()
        .enumerate()
        .map(|(index, d)| {
            let change = crate::localization::text(match d.status {
                DifferenceStatus::MissingInTarget => "explorer.schema_compare.change_missing",
                DifferenceStatus::ExtraInTarget => "explorer.schema_compare.change_extra",
                DifferenceStatus::Changed => "explorer.schema_compare.change_changed",
                DifferenceStatus::Unsupported => "explorer.schema_compare.change_unsupported",
                DifferenceStatus::ManualReview => "explorer.schema_compare.change_manual",
                DifferenceStatus::Renamed { .. } => "explorer.schema_compare.change_renamed",
            });
            let risk = risks.get(index).copied().unwrap_or(Risk::Unknown);
            vec![
                CellValue::String(change),
                CellValue::String(d.object.kind.clone()),
                CellValue::String(format!("{}.{}", d.object.schema, d.object.name)),
                CellValue::String(d.note.clone()),
                CellValue::String(risk.label().to_string()),
            ]
        })
        .collect();
    ToolQuerySet { columns, rows }
}

#[cfg(not(target_arch = "wasm32"))]
fn diff_grid_rows(target_def: &str, source_def: &str) -> Vec<(String, String, String)> {
    let diff = diff_lines_capped(target_def, source_def, 2_000);
    diff.lines
        .into_iter()
        .map(|line| match line.kind {
            DiffKind::Added => (line.text, String::new(), "+".to_string()),
            DiffKind::Removed => (String::new(), line.text, "-".to_string()),
            DiffKind::Same => (line.text.clone(), line.text, String::new()),
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn build_definition_diff_grid(
    target_def: &str,
    source_def: &str,
    cx: &mut gpui::App,
) -> Option<Entity<SqllyDataTable>> {
    let columns = vec![
        Column::new(
            crate::localization::text("explorer.schema_compare.source"),
            ColumnKind::Text,
            320.0,
        ),
        Column::new(
            crate::localization::text("explorer.schema_compare.target"),
            ColumnKind::Text,
            320.0,
        ),
        Column::new(
            crate::localization::text("explorer.schema_compare.column_change"),
            ColumnKind::Text,
            48.0,
        ),
    ];
    let rows = diff_grid_rows(target_def, source_def)
        .into_iter()
        .map(|(source, target, change)| {
            vec![
                if source.is_empty() {
                    GridCell::None
                } else {
                    GridCell::Text(source)
                },
                if target.is_empty() {
                    GridCell::None
                } else {
                    GridCell::Text(target)
                },
                if change.is_empty() {
                    GridCell::None
                } else {
                    GridCell::Text(change)
                },
            ]
        })
        .collect();
    let data = GridData::new(columns, rows).ok()?;
    let theme = crate::grid_bridge::build_grid_theme();
    let view = SqllyDataTable::builder(data)
        .config(Default::default())
        .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
        .theme(theme)
        .build(cx);
    Some(cx.new(|_cx| view))
}

fn rebuild_differences_from_raw(
    raw: &[ComparedDifference],
    rename_candidates: &[(RenameCandidate, bool)],
) -> Vec<ComparedDifference> {
    let mut differences = raw.to_vec();
    let accepted = rename_candidates
        .iter()
        .filter(|(_, accepted)| *accepted)
        .map(|(candidate, _)| candidate.clone())
        .collect::<Vec<_>>();
    apply_renames(&mut differences, &accepted);
    differences
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct DeployRequest {
    pub profile: ConnectionProfile,
    pub script: String,
    pub model: crate::schema_compare_deploy::DeployModel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComparePhase {
    Idle,
    LoadingSource,
    LoadingTarget,
    Done,
}

impl EventEmitter<ToolDialogEvent> for SchemaComparePane {}

pub struct SchemaComparePane {
    router: ServiceRouter,
    profiles: Vec<ConnectionProfile>,
    /// Saved schema snapshots, offered after the live connections in both
    /// side pickers.
    snapshots: Vec<SchemaSnapshotMeta>,
    source_profile: Option<ConnectionProfile>,
    target_profile: Option<ConnectionProfile>,
    /// A side reading a saved snapshot instead of a live database. Exactly
    /// one of `*_profile` / `*_snapshot` is set per side.
    source_snapshot: Option<SchemaSnapshotMeta>,
    target_snapshot: Option<SchemaSnapshotMeta>,
    source_profile_select: Entity<SelectState<Vec<String>>>,
    target_profile_select: Entity<SelectState<Vec<String>>>,
    source_databases: Vec<String>,
    target_databases: Vec<String>,
    source_databases_loading: bool,
    target_databases_loading: bool,
    source_db: Option<String>,
    target_db: Option<String>,
    source_database_select: Entity<SelectState<Vec<String>>>,
    target_database_select: Entity<SelectState<Vec<String>>>,
    selects_dirty: bool,
    phase: ComparePhase,
    source_objects: Vec<SchemaObject>,
    raw_differences: Vec<ComparedDifference>,
    differences: Vec<ComparedDifference>,
    /// Risk per entry of `differences`, kept parallel so deploy keeps reading
    /// `ComparedDifference` untouched.
    risks: Vec<Risk>,
    /// Grid display order: indices into `differences`, riskiest first.
    display_order: Vec<usize>,
    /// The optional decision provider; `None` hides the Assess risk button.
    decision: Option<sqlly_ai::catalog::DynDecisionProvider>,
    assessing: bool,
    rename_candidates: Vec<(RenameCandidate, bool)>,
    deploy_options: DeployOptions,
    grid: Option<Entity<SqllyDataTable>>,
    diff_grid: Option<Entity<SqllyDataTable>>,
    diff_row: Option<usize>,
    #[cfg(not(target_arch = "wasm32"))]
    deploy_request: Option<DeployRequest>,
    error: Option<String>,
    generation: u64,
    source_database_generation: u64,
    target_database_generation: u64,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

fn profile_label(profile: &ConnectionProfile) -> String {
    let id = profile.id.0.to_string();
    format!(
        "{} · {} · {}:{} · {}",
        profile.name,
        profile.database_type.display_name(),
        profile.server,
        profile.port,
        &id[..8]
    )
}

/// What a row of the connection/snapshot picker refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerEntry {
    Profile(usize),
    Snapshot(usize),
}

/// The picker lists the live connections first, then the saved snapshots.
fn picker_entry(profiles: usize, snapshots: usize, row: Option<usize>) -> Option<PickerEntry> {
    let row = row?;
    if row < profiles {
        Some(PickerEntry::Profile(row))
    } else if row - profiles < snapshots {
        Some(PickerEntry::Snapshot(row - profiles))
    } else {
        None
    }
}

impl SchemaComparePane {
    pub fn new(
        context: ToolDialogContext,
        router: ServiceRouter,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let mut profiles = ConnectionStore::global()
            .list()
            .into_iter()
            .filter(|profile| collect_objects_sql(profile.database_type, "probe").is_ok())
            .take(MAX_COMPARE_PROFILES)
            .collect::<Vec<_>>();
        if !profiles
            .iter()
            .any(|profile| profile.id == context.profile.id)
        {
            profiles.push(context.profile.clone());
        }
        profiles.sort_by_key(profile_label);
        let source_profile = profiles
            .iter()
            .find(|profile| profile.id == context.profile.id)
            .cloned();
        let target_profile = profiles
            .iter()
            .find(|profile| Some(profile.id) != source_profile.as_ref().map(|source| source.id))
            .cloned()
            .or_else(|| source_profile.clone());
        let source_profile_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let target_profile_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let source_database_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let target_database_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &source_profile_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    let row = value
                        .as_ref()
                        .and_then(|_| select.read(cx).selected_index(cx))
                        .map(|ix| ix.row);
                    this.choose_side(CompareSide::Source, row, cx);
                },
            ),
            cx.subscribe_in(
                &target_profile_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    let row = value
                        .as_ref()
                        .and_then(|_| select.read(cx).selected_index(cx))
                        .map(|ix| ix.row);
                    this.choose_side(CompareSide::Target, row, cx);
                },
            ),
            cx.subscribe_in(
                &source_database_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.source_db = value.as_ref().map(|value| value.to_string());
                    this.invalidate_comparison();
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &target_database_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.target_db = value.as_ref().map(|value| value.to_string());
                    this.invalidate_comparison();
                    cx.notify();
                },
            ),
        ];
        let mut this = Self {
            router,
            profiles,
            snapshots: list_snapshots(),
            source_profile,
            target_profile,
            source_snapshot: None,
            target_snapshot: None,
            source_profile_select,
            target_profile_select,
            source_databases: Vec::new(),
            target_databases: Vec::new(),
            source_databases_loading: false,
            target_databases_loading: false,
            source_db: None,
            target_db: None,
            source_database_select,
            target_database_select,
            selects_dirty: true,
            phase: ComparePhase::Idle,
            source_objects: Vec::new(),
            raw_differences: Vec::new(),
            differences: Vec::new(),
            risks: Vec::new(),
            display_order: Vec::new(),
            decision: crate::decision_ai::decision_provider(),
            assessing: false,
            rename_candidates: Vec::new(),
            deploy_options: DeployOptions::default(),
            grid: None,
            diff_grid: None,
            diff_row: None,
            #[cfg(not(target_arch = "wasm32"))]
            deploy_request: None,
            error: None,
            generation: 0,
            source_database_generation: 0,
            target_database_generation: 0,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(940.0), Theme::scaled(600.0)),
            _subscriptions: subscriptions,
        };
        this.load_databases(CompareSide::Source, cx);
        this.load_databases(CompareSide::Target, cx);
        this
    }

    #[cfg(not(target_arch = "wasm32"))]
    /// Open with both sides preset (Schema History's Compare) and the
    /// comparison already running, so the pane lands in its diff state.
    pub fn new_with_sources(
        context: ToolDialogContext,
        router: ServiceRouter,
        source: CompareSource,
        target: CompareSource,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let mut this = Self::new(context, router, window, cx);
        this.set_side(CompareSide::Source, source, cx);
        this.set_side(CompareSide::Target, target, cx);
        this.compare(cx);
        this
    }

    #[cfg(not(target_arch = "wasm32"))]
    /// Re-read the saved snapshots (after one was taken while the pane is
    /// open) so the pickers offer it.
    pub fn reload_snapshots(&mut self, cx: &mut gpui::Context<Self>) {
        self.snapshots = list_snapshots();
        self.selects_dirty = true;
        cx.notify();
    }

    /// Point a side at a live database or a saved snapshot.
    fn set_side(&mut self, side: CompareSide, source: CompareSource, cx: &mut gpui::Context<Self>) {
        self.invalidate_comparison();
        match source {
            CompareSource::Live { profile, database } => {
                let profile = *profile;
                if !self.profiles.iter().any(|known| known.id == profile.id) {
                    self.profiles.push(profile.clone());
                }
                match side {
                    CompareSide::Source => {
                        self.source_snapshot = None;
                        self.source_profile = Some(profile);
                        self.source_db = Some(database);
                    }
                    CompareSide::Target => {
                        self.target_snapshot = None;
                        self.target_profile = Some(profile);
                        self.target_db = Some(database);
                    }
                }
                self.load_databases(side, cx);
            }
            CompareSource::Snapshot(meta) => {
                if !self.snapshots.iter().any(|known| known.id == meta.id) {
                    self.snapshots.push(meta.clone());
                }
                let database = meta.database.clone();
                match side {
                    CompareSide::Source => {
                        self.source_profile = None;
                        self.source_snapshot = Some(meta);
                        self.source_databases = vec![database.clone()];
                        self.source_db = Some(database);
                        self.source_databases_loading = false;
                        self.source_database_generation += 1;
                    }
                    CompareSide::Target => {
                        self.target_profile = None;
                        self.target_snapshot = Some(meta);
                        self.target_databases = vec![database.clone()];
                        self.target_db = Some(database);
                        self.target_databases_loading = false;
                        self.target_database_generation += 1;
                    }
                }
            }
        }
        self.selects_dirty = true;
        cx.notify();
    }

    /// A picker confirmation: the row names either a live connection or a
    /// saved snapshot (snapshots follow the connections in the picker).
    fn choose_side(&mut self, side: CompareSide, row: Option<usize>, cx: &mut gpui::Context<Self>) {
        let Some(entry) = picker_entry(self.profiles.len(), self.snapshots.len(), row) else {
            return;
        };
        if let PickerEntry::Profile(index) = entry {
            let profile = self.profiles[index].clone();
            let database = profile.database.clone().unwrap_or_default();
            self.set_side(
                side,
                CompareSource::Live {
                    profile: Box::new(profile),
                    database,
                },
                cx,
            );
        } else if let PickerEntry::Snapshot(index) = entry {
            let meta = self.snapshots[index].clone();
            self.set_side(side, CompareSource::Snapshot(meta), cx);
        }
    }

    /// The side as a comparison source, if fully chosen.
    fn side_source(&self, side: CompareSide) -> Option<CompareSource> {
        let (profile, snapshot, database) = match side {
            CompareSide::Source => (&self.source_profile, &self.source_snapshot, &self.source_db),
            CompareSide::Target => (&self.target_profile, &self.target_snapshot, &self.target_db),
        };
        if let Some(meta) = snapshot {
            return Some(CompareSource::Snapshot(meta.clone()));
        }
        let profile = Box::new(profile.clone()?);
        let database = database.clone()?;
        Some(CompareSource::Live { profile, database })
    }

    fn side_type(&self, side: CompareSide) -> Option<DatabaseType> {
        self.side_source(side).map(|source| source.database_type())
    }

    /// The live target with its database pinned — what deploy and the
    /// scripts run against. `None` when the target is a snapshot.
    fn live_target(&self) -> Option<ConnectionProfile> {
        match self.side_source(CompareSide::Target)? {
            CompareSource::Live {
                mut profile,
                database,
            } => {
                profile.database = Some(database);
                Some(*profile)
            }
            CompareSource::Snapshot(_) => None,
        }
    }

    fn invalidate_comparison(&mut self) {
        self.generation += 1;
        self.phase = ComparePhase::Idle;
        self.source_objects.clear();
        self.raw_differences.clear();
        self.differences.clear();
        self.risks.clear();
        self.display_order.clear();
        self.assessing = false;
        self.rename_candidates.clear();
        self.grid = None;
        self.diff_grid = None;
        self.diff_row = None;
        self.error = None;
    }

    fn apply_rename_toggles(&mut self, cx: &mut gpui::Context<Self>) {
        self.differences =
            rebuild_differences_from_raw(&self.raw_differences, &self.rename_candidates);
        self.refresh_risks();
        self.rebuild_diff_grid(cx);
        self.diff_grid = None;
        self.diff_row = None;
        cx.notify();
    }

    /// Reset every difference to its deterministic base risk. Any
    /// decision-model classification is discarded, which is the safe
    /// direction: rows fall back to `Unknown`, never to an invented risk.
    fn refresh_risks(&mut self) {
        self.risks = self.differences.iter().map(base_risk).collect();
    }

    fn rebuild_diff_grid(&mut self, cx: &mut gpui::Context<Self>) {
        self.display_order =
            crate::schema_compare_risk::display_order(&self.risks, &self.differences);
        let ordered_differences = self
            .display_order
            .iter()
            .map(|&index| self.differences[index].clone())
            .collect::<Vec<_>>();
        let ordered_risks = self
            .display_order
            .iter()
            .map(|&index| self.risks[index])
            .collect::<Vec<_>>();
        self.grid = crate::tool_dialog::build_result_grid(
            &diff_grid_set(&ordered_differences, &ordered_risks),
            cx,
        );
    }

    fn finish_compare(
        &mut self,
        source_type: DatabaseType,
        target_type: DatabaseType,
        target_objects: Vec<SchemaObject>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.raw_differences = compare_objects(
            &self.source_objects,
            &target_objects,
            source_type,
            target_type,
        );
        self.rename_candidates = detect_renames(&self.raw_differences)
            .into_iter()
            .map(|candidate| {
                let accepted = candidate.similarity_percent == 100;
                (candidate, accepted)
            })
            .collect();
        self.differences =
            rebuild_differences_from_raw(&self.raw_differences, &self.rename_candidates);
        self.refresh_risks();
        self.rebuild_diff_grid(cx);
        self.diff_grid = None;
        self.phase = ComparePhase::Done;
    }

    /// The Jev client is native-only and the browser build never has a
    /// provider, so the button that calls this cannot appear there.
    #[cfg(target_arch = "wasm32")]
    fn assess_risk(&mut self, _cx: &mut gpui::Context<Self>) {}

    /// Run the optional decision-model pass over `Changed` definition pairs.
    /// Fire-and-forget: the compare, scripts, and deploy never wait on it,
    /// and any error leaves every risk at its deterministic base value.
    #[cfg(not(target_arch = "wasm32"))]
    fn assess_risk(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(provider) = self.decision.clone() else {
            return;
        };
        if self.assessing {
            return;
        }
        let differences = self.differences.clone();
        if crate::schema_compare_risk::assessment_batches(&differences).is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.schema_compare.no_changes_to_assess"),
            });
            return;
        }
        self.assessing = true;
        let generation = self.generation;
        cx.notify();

        type AssessOutcome = (Vec<(usize, Risk)>, Option<String>);
        let (tx, rx) = tokio::sync::oneshot::channel::<AssessOutcome>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let outcome =
                        crate::schema_compare_risk::assess_changed(provider.as_ref(), &differences)
                            .await;
                    let _ = tx.send(outcome);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok((results, error)) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        if this.generation != generation {
                            return;
                        }
                        this.assessing = false;
                        for (index, risk) in results {
                            if let Some(slot) = this.risks.get_mut(index) {
                                *slot = risk;
                            }
                        }
                        this.rebuild_diff_grid(cx);
                        this.diff_grid = None;
                        this.diff_row = None;
                        if let Some(error) = error {
                            cx.emit(ToolDialogEvent::Status {
                                level: StatusLevel::Info,
                                message: crate::localization::format(
                                    "explorer.schema_compare.risk_incomplete",
                                    &[("error", &error)],
                                ),
                            });
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn take_deploy_request(&mut self) -> Option<DeployRequest> {
        self.deploy_request.take()
    }

    fn load_databases(&mut self, side: CompareSide, cx: &mut gpui::Context<Self>) {
        let profile = match side {
            CompareSide::Source => self.source_profile.clone(),
            CompareSide::Target => self.target_profile.clone(),
        };
        let Some(profile) = profile else { return };
        let generation = match side {
            CompareSide::Source => {
                self.source_database_generation += 1;
                self.source_databases_loading = true;
                self.source_database_generation
            }
            CompareSide::Target => {
                self.target_database_generation += 1;
                self.target_databases_loading = true;
                self.target_database_generation
            }
        };
        let Some(sql) = database_list_sql(profile.database_type) else {
            let database = profile
                .database
                .clone()
                .unwrap_or_else(|| profile.server.clone());
            self.apply_databases(side, generation, vec![database], None, cx);
            return;
        };
        let context = ToolDialogContext::new(profile.clone(), self.router.services_for(&profile));
        crate::tool_dialog::run_query(&context, sql.to_string(), cx, move |this, outcome, cx| {
            let databases = outcome
                .sets
                .first()
                .map(|set| {
                    set.rows
                        .iter()
                        .filter_map(|row| match row.first() {
                            Some(CellValue::String(value)) => Some(value.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            this.apply_databases(side, generation, databases, outcome.error, cx);
        });
    }

    fn apply_databases(
        &mut self,
        side: CompareSide,
        generation: u64,
        databases: Vec<String>,
        error: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        let (current_generation, loading, values, selected, profile) = match side {
            CompareSide::Source => (
                &self.source_database_generation,
                &mut self.source_databases_loading,
                &mut self.source_databases,
                &mut self.source_db,
                self.source_profile.as_ref(),
            ),
            CompareSide::Target => (
                &self.target_database_generation,
                &mut self.target_databases_loading,
                &mut self.target_databases,
                &mut self.target_db,
                self.target_profile.as_ref(),
            ),
        };
        if *current_generation != generation {
            return;
        }
        *loading = false;
        if let Some(error) = error {
            let connection = profile
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| crate::localization::text("explorer.schema_compare.connection"));
            self.error = Some(crate::localization::format(
                "explorer.schema_compare.list_databases_failed",
                &[("connection", &connection), ("error", &error)],
            ));
        } else {
            *values = databases;
            *selected = profile
                .and_then(|profile| profile.database.clone())
                .filter(|database| values.contains(database))
                .or_else(|| values.first().cloned());
            self.error = None;
        }
        self.selects_dirty = true;
        cx.notify();
    }

    /// Resolve what one side reads: a snapshot side names the file to load,
    /// a live side becomes the metadata batch to run.
    fn side_input(&self, side: CompareSide) -> Result<SideInput, String> {
        let Some(source) = self.side_source(side) else {
            return Err(crate::localization::text(
                "explorer.schema_compare.pick_databases",
            ));
        };
        match source {
            CompareSource::Snapshot(meta) => Ok(SideInput::Snapshot(meta)),
            CompareSource::Live {
                mut profile,
                database,
            } => {
                let sql = collect_objects_sql(profile.database_type, &database)?;
                profile.database = Some(database);
                let services = self.router.services_for(&profile);
                let context = Box::new(ToolDialogContext::new(*profile, services));
                Ok(SideInput::Query { context, sql })
            }
        }
    }

    /// Both sides name the same thing: nothing to compare.
    fn sides_identical(&self) -> bool {
        match (
            self.side_source(CompareSide::Source),
            self.side_source(CompareSide::Target),
        ) {
            (
                Some(CompareSource::Live {
                    profile: a,
                    database: da,
                }),
                Some(CompareSource::Live {
                    profile: b,
                    database: db,
                }),
            ) => a.id == b.id && da == db,
            (Some(CompareSource::Snapshot(a)), Some(CompareSource::Snapshot(b))) => a.id == b.id,
            _ => false,
        }
    }

    /// Produce one side's objects — immediately for a snapshot, after the
    /// bounded metadata batch for a live database — and hand them on.
    /// Failures land in `error` and reset the phase; `on_done` never runs.
    fn run_side(
        &mut self,
        side: CompareSide,
        input: SideInput,
        generation: u64,
        on_done: impl FnOnce(&mut Self, Vec<SchemaObject>, &mut gpui::Context<Self>) + 'static,
        cx: &mut gpui::Context<Self>,
    ) {
        let side_name = crate::localization::text(match side {
            CompareSide::Source => "explorer.schema_compare.source",
            CompareSide::Target => "explorer.schema_compare.target",
        });
        match input {
            SideInput::Snapshot(meta) => {
                let title = snapshot_title(&meta);
                let read = cx.background_spawn(async move { load_snapshot_objects(&meta) });
                cx.spawn(async move |this, cx| {
                    let result = read.await;
                    this.update(cx, |this, cx| {
                        if this.generation != generation {
                            return;
                        }
                        match result {
                            Ok(objects) => on_done(this, objects, cx),
                            Err(error) => {
                                this.error = Some(crate::localization::format(
                                    "explorer.schema_compare.read_snapshot_failed",
                                    &[
                                        ("title", title.as_str()),
                                        ("error", crate::localization::message(error).as_str()),
                                    ],
                                ));
                                this.phase = ComparePhase::Idle;
                                cx.notify();
                            }
                        }
                    })
                    .ok();
                })
                .detach();
            }
            SideInput::Query { context, sql } => {
                crate::tool_dialog::run_query(&context, sql, cx, move |this, outcome, cx| {
                    if this.generation != generation {
                        return;
                    }
                    if let Some(err) = outcome.error {
                        this.error = Some(crate::localization::format(
                            "explorer.schema_compare.load_side_failed",
                            &[("side", &side_name), ("error", &err)],
                        ));
                        this.phase = ComparePhase::Idle;
                        cx.notify();
                        return;
                    }
                    if collection_limit_reached(&outcome) {
                        this.error = Some(crate::localization::format(
                            "explorer.schema_compare.collection_limit",
                            &[
                                ("side", &side_name),
                                ("cap", &TOOL_QUERY_ROW_CAP.to_string()),
                            ],
                        ));
                        this.phase = ComparePhase::Idle;
                        cx.notify();
                        return;
                    }
                    on_done(this, objects_from_outcome(&outcome), cx);
                });
            }
        }
    }

    fn compare(&mut self, cx: &mut gpui::Context<Self>) {
        let (Some(source_type), Some(target_type)) = (
            self.side_type(CompareSide::Source),
            self.side_type(CompareSide::Target),
        ) else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.schema_compare.pick_databases"),
            });
            return;
        };
        if self.sides_identical() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.schema_compare.same_database"),
            });
            return;
        }
        let (source_input, target_input) = match (
            self.side_input(CompareSide::Source),
            self.side_input(CompareSide::Target),
        ) {
            (Ok(source), Ok(target)) => (source, target),
            (Err(error), _) | (_, Err(error)) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.phase = ComparePhase::LoadingSource;
        self.error = None;
        self.grid = None;
        self.differences.clear();
        self.generation += 1;
        let generation = self.generation;
        self.run_side(
            CompareSide::Source,
            source_input,
            generation,
            move |this, source_objects, cx| {
                this.source_objects = source_objects;
                this.phase = ComparePhase::LoadingTarget;
                this.run_side(
                    CompareSide::Target,
                    target_input,
                    generation,
                    move |this, target_objects, cx| {
                        this.finish_compare(source_type, target_type, target_objects, cx);
                        cx.notify();
                    },
                    cx,
                );
                cx.notify();
            },
            cx,
        );
        cx.notify();
    }

    fn side_label(&self, side: CompareSide) -> String {
        self.side_source(side)
            .map(|source| source.display_name())
            .unwrap_or_default()
    }

    fn compare_header(&self, kind: &str) -> String {
        format!(
            "-- Schema compare {kind}: {} / [{}] (source) vs {} / [{}] (target)\n\
             -- Review carefully; target-only drops are {}.\n\n",
            self.side_label(CompareSide::Source),
            self.source_db.as_deref().unwrap_or(""),
            self.side_label(CompareSide::Target),
            self.target_db.as_deref().unwrap_or(""),
            if self.deploy_options.include_drops {
                "included when enabled"
            } else {
                "off unless you enable Include DROP"
            }
        )
    }

    fn deploy_plan(&self) -> sqlly_services::DeployPlan {
        let source_type = self
            .side_type(CompareSide::Source)
            .unwrap_or(DatabaseType::SqlServer);
        let target_type = self
            .side_type(CompareSide::Target)
            .unwrap_or(DatabaseType::SqlServer);
        plan_deploy(
            &self.differences,
            source_type,
            target_type,
            self.deploy_options,
        )
    }

    /// Hand a generated script to an editor tab: pinned to the live target
    /// when there is one, otherwise (snapshot target) a plain review tab.
    fn script_to_editor(&mut self, sql: String, cx: &mut gpui::Context<Self>) {
        match self.live_target() {
            Some(profile) => cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
                sql,
                profile: Box::new(profile),
            }),
            None => cx.emit(ToolDialogEvent::ScriptToEditor { sql }),
        }
        cx.emit(ToolDialogEvent::Close);
    }

    fn deploy_script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        if self.differences.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.schema_compare.compare_required"),
            });
            return;
        }
        let header = self.compare_header("DEPLOY");
        let sql = deploy_script(&self.deploy_plan(), &header);
        self.script_to_editor(sql, cx);
    }

    fn rollback_script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        if self.differences.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.schema_compare.compare_required"),
            });
            return;
        }
        let header = self.compare_header("ROLLBACK");
        let sql = rollback_script(&self.deploy_plan(), &header);
        self.script_to_editor(sql, cx);
    }

    /// Ask the app to open the Save Schema Snapshot dialog for one live side.
    fn snapshot_side(&mut self, side: CompareSide, cx: &mut gpui::Context<Self>) {
        let Some(CompareSource::Live {
            mut profile,
            database,
        }) = self.side_source(side)
        else {
            return;
        };
        if database.is_empty() {
            return;
        }
        profile.database = Some(database);
        cx.emit(ToolDialogEvent::SaveSchemaSnapshot { profile });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn deploy(&mut self, cx: &mut gpui::Context<Self>) {
        if self.differences.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.schema_compare.compare_required"),
            });
            return;
        }
        let Some(target_profile) = self.live_target() else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text(
                    "explorer.schema_compare.deploy_snapshot_target",
                ),
            });
            return;
        };
        let plan = self.deploy_plan();
        let script = deploy_script(&plan, &self.compare_header("DEPLOY"));
        let store = ConnectionStore::global();
        let safety =
            crate::mutation_review::SafetyFeatures::effective_for_profile(&target_profile, store);
        let model = crate::schema_compare_deploy::DeployModel::from_plan(
            &plan,
            target_profile.database_type,
            safety.require_ai_review_for_data_changes,
        );
        self.deploy_request = Some(DeployRequest {
            profile: target_profile,
            script,
            model,
        });
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Info,
            message: "deploy-requested".to_string(),
        });
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }
}

impl gpui::Focusable for SchemaComparePane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SchemaComparePane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.selects_dirty = false;
            // Live connections first, then saved snapshots ("Snapshot: label
            // @ time · database"); a snapshot's index is offset past the
            // connections.
            let profile_items = self
                .profiles
                .iter()
                .map(profile_label)
                .chain(self.snapshots.iter().map(picker_label))
                .collect::<Vec<_>>();
            let side_index = |profile: &Option<ConnectionProfile>,
                              snapshot: &Option<SchemaSnapshotMeta>| {
                if let Some(meta) = snapshot {
                    return self
                        .snapshots
                        .iter()
                        .position(|candidate| candidate.id == meta.id)
                        .map(|index| gpui_component::IndexPath::new(self.profiles.len() + index));
                }
                profile
                    .as_ref()
                    .and_then(|selected| {
                        self.profiles
                            .iter()
                            .position(|candidate| candidate.id == selected.id)
                    })
                    .map(gpui_component::IndexPath::new)
            };
            let source_profile_index = side_index(&self.source_profile, &self.source_snapshot);
            let target_profile_index = side_index(&self.target_profile, &self.target_snapshot);
            let source_database_index = self
                .source_db
                .as_ref()
                .and_then(|database| {
                    self.source_databases
                        .iter()
                        .position(|candidate| candidate == database)
                })
                .map(gpui_component::IndexPath::new);
            let target_database_index = self
                .target_db
                .as_ref()
                .and_then(|database| {
                    self.target_databases
                        .iter()
                        .position(|candidate| candidate == database)
                })
                .map(gpui_component::IndexPath::new);
            self.source_profile_select.update(cx, |state, cx| {
                state.set_items(profile_items.clone(), window, cx);
                state.set_selected_index(source_profile_index, window, cx);
            });
            self.target_profile_select.update(cx, |state, cx| {
                state.set_items(profile_items, window, cx);
                state.set_selected_index(target_profile_index, window, cx);
            });
            self.source_database_select.update(cx, |state, cx| {
                state.set_items(self.source_databases.clone(), window, cx);
                state.set_selected_index(source_database_index, window, cx);
            });
            self.target_database_select.update(cx, |state, cx| {
                state.set_items(self.target_databases.clone(), window, cx);
                state.set_selected_index(target_database_index, window, cx);
            });
        }

        let busy = matches!(
            self.phase,
            ComparePhase::LoadingSource | ComparePhase::LoadingTarget
        );
        let can_snapshot = |this: &Self, side: CompareSide| {
            !busy
                && matches!(
                    this.side_source(side),
                    Some(CompareSource::Live { database, .. }) if !database.is_empty()
                )
        };
        let can_snapshot_source = can_snapshot(self, CompareSide::Source);
        let can_snapshot_target = can_snapshot(self, CompareSide::Target);
        #[cfg(not(target_arch = "wasm32"))]
        let target_is_snapshot = self.target_snapshot.is_some();

        let content: gpui::AnyElement = if let Some(err) = &self.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(crate::localization::format(
                    "explorer.schema_compare.error_with_permission",
                    &[("error", err)],
                ))
                .into_any_element()
        } else if busy {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(match self.phase {
                    ComparePhase::LoadingSource => "explorer.schema_compare.reading_source",
                    _ => "explorer.schema_compare.reading_target",
                }))
                .into_any_element()
        } else if self.phase == ComparePhase::Done {
            if self.differences.is_empty() {
                div()
                    .p(px(s(12.0)))
                    .text_color(Theme::text())
                    .child(crate::localization::text(
                        "explorer.schema_compare.no_differences",
                    ))
                    .into_any_element()
            } else {
                let mut body = div().size_full().flex().flex_col().gap(px(s(6.0)));
                if !self.rename_candidates.is_empty() {
                    let mut strip = div().flex().flex_col().gap(px(s(4.0))).child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.schema_compare.suggested_renames",
                            )),
                    );
                    for (index, (candidate, accepted)) in self.rename_candidates.iter().enumerate()
                    {
                        let id = format!("sc-rename-{index}");
                        let label = format!(
                            "{} {}.{} → {} ({}%)",
                            candidate.kind,
                            candidate.schema,
                            candidate.from_name,
                            candidate.to_name,
                            candidate.similarity_percent
                        );
                        let pane = cx.entity().downgrade();
                        strip = strip.child(
                            Checkbox::new(id).checked(*accepted).label(label).on_click(
                                move |checked: &bool, _window, app| {
                                    let checked = *checked;
                                    if let Some(pane) = pane.upgrade() {
                                        pane.update(app, |this, cx| {
                                            if let Some(entry) =
                                                this.rename_candidates.get_mut(index)
                                            {
                                                entry.1 = checked;
                                            }
                                            this.apply_rename_toggles(cx);
                                        });
                                    }
                                },
                            ),
                        );
                    }
                    body = body.child(strip);
                }
                body = body.child(
                    Checkbox::new("sc-include-drops")
                        .checked(self.deploy_options.include_drops)
                        .label(crate::localization::text(
                            "explorer.schema_compare.include_drops",
                        ))
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                            this.deploy_options.include_drops = *checked;
                            cx.notify();
                        })),
                );
                if let Some(grid) = &self.grid {
                    body = body.child(div().flex_1().min_h(px(0.0)).child(grid.clone()));
                }
                let selected_row = self
                    .grid
                    .as_ref()
                    .and_then(|grid| crate::tool_dialog::selected_source_row(grid, cx));
                if selected_row != self.diff_row {
                    self.diff_row = selected_row;
                    self.diff_grid = None;
                }
                if let Some(row_index) = selected_row {
                    // Grid rows are in risk-sorted display order; map back to
                    // the difference the row shows.
                    let selected = self
                        .display_order
                        .get(row_index)
                        .and_then(|&index| self.differences.get(index));
                    if let Some(selected) = selected {
                        let show_diff = matches!(
                            selected.status,
                            DifferenceStatus::Changed
                                | DifferenceStatus::ManualReview
                                | DifferenceStatus::Renamed { .. }
                        ) && selected.target_definition.is_some();
                        if show_diff {
                            let source_def = &selected.object.definition;
                            let target_def = selected.target_definition.as_deref().unwrap_or("");
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                if self.diff_grid.is_none() {
                                    self.diff_grid =
                                        build_definition_diff_grid(target_def, source_def, cx);
                                }
                                if let Some(diff_grid) = &self.diff_grid {
                                    body = body.child(
                                        div()
                                            .h(px(s(160.0)))
                                            .border_t_1()
                                            .border_color(Theme::border())
                                            .pt(px(s(6.0)))
                                            .flex()
                                            .flex_col()
                                            .gap(px(s(4.0)))
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .gap(px(s(8.0)))
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .text_color(Theme::text_dim())
                                                            .child(crate::localization::text(
                                                                "explorer.schema_compare.source",
                                                            )),
                                                    )
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .text_color(Theme::text_dim())
                                                            .child(crate::localization::text(
                                                                "explorer.schema_compare.target",
                                                            )),
                                                    ),
                                            )
                                            .child(diff_grid.clone()),
                                    );
                                }
                            }
                            #[cfg(target_arch = "wasm32")]
                            {
                                body = body.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(s(8.0)))
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_size(px(Theme::font_size_ratio(0.8)))
                                                .text_color(Theme::text())
                                                .child(source_def.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_size(px(Theme::font_size_ratio(0.8)))
                                                .text_color(Theme::text_dim())
                                                .child(target_def.to_string()),
                                        ),
                                );
                            }
                        } else {
                            self.diff_grid = None;
                            self.diff_row = None;
                        }
                    } else {
                        self.diff_grid = None;
                        self.diff_row = None;
                    }
                } else {
                    self.diff_grid = None;
                    self.diff_row = None;
                }
                body.into_any_element()
            }
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.schema_compare.pick_prompt",
                ))
                .into_any_element()
        };

        // Bare body: the diff grids manage their own scroll, so the dialog
        // section keeps its own padding instead of the chrome's.
        let body =
            div()
                .size_full()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .p(px(s(14.0)))
                .text_size(px(Theme::font_size()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(div().w(px(s(52.0))).text_color(Theme::text_dim()).child(
                                    crate::localization::text("explorer.schema_compare.source"),
                                ))
                                .child(
                                    div()
                                        .w(px(s(430.0)))
                                        .child(Select::new(&self.source_profile_select).w_full()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Select::new(&self.source_database_select).w_full()),
                                )
                                .child(
                                    Button::new("sc-snapshot-source")
                                        .ghost()
                                        .xsmall()
                                        .label(crate::localization::text(
                                            "explorer.schema_compare.snapshot_source_button",
                                        ))
                                        .disabled(!can_snapshot_source)
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.snapshot_side(CompareSide::Source, cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(div().w(px(s(52.0))).text_color(Theme::text_dim()).child(
                                    crate::localization::text("explorer.schema_compare.target"),
                                ))
                                .child(
                                    div()
                                        .w(px(s(430.0)))
                                        .child(Select::new(&self.target_profile_select).w_full()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Select::new(&self.target_database_select).w_full()),
                                )
                                .child(
                                    Button::new("sc-snapshot-target")
                                        .ghost()
                                        .xsmall()
                                        .label(crate::localization::text(
                                            "explorer.schema_compare.snapshot_target_button",
                                        ))
                                        .disabled(!can_snapshot_target)
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.snapshot_side(CompareSide::Target, cx)
                                        })),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(4.0)))
                        .child(content),
                );

        let focus_handle = self.focus_handle.clone();
        let dialog = Dialog::new("schema-compare-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("explorer.schema_compare.title"))
            .key_context("SchemaComparePane")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .bare_body()
            .child(body)
            .leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.schema_compare.differences_summary",
                        &[("count", &self.differences.len().to_string())],
                    )),
            )
            .leading_button(
                Button::new("sc-rollback")
                    .label(crate::localization::text(
                        "explorer.schema_compare.rollback_button",
                    ))
                    .disabled(self.differences.is_empty())
                    .on_click(
                        cx.listener(|this, _, _window, cx| this.rollback_script_to_editor(cx)),
                    ),
            );
        // The decision-model pass is opt-in; without a configured provider
        // (always the case in the browser build) the button does not exist
        // and behaviour is exactly the deterministic base classification.
        let dialog = if self.decision.is_some() {
            dialog.leading_button(
                Button::new("sc-assess")
                    .label(crate::localization::text(if self.assessing {
                        "explorer.schema_compare.assessing_button"
                    } else {
                        "explorer.schema_compare.assess_button"
                    }))
                    .disabled(self.differences.is_empty() || self.assessing)
                    .on_click(cx.listener(|this, _, _window, cx| this.assess_risk(cx))),
            )
        } else {
            dialog
        };
        let dialog = dialog
            .leading_button(
                Button::new("sc-script")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.schema_compare.deploy_script_button",
                    ))
                    .disabled(self.differences.is_empty())
                    .on_click(cx.listener(|this, _, _window, cx| this.deploy_script_to_editor(cx))),
            )
            .button(
                Button::new("sc-compare")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.schema_compare.compare_button",
                    ))
                    .disabled(
                        busy || self.source_databases_loading || self.target_databases_loading,
                    )
                    .on_click(cx.listener(|this, _, _window, cx| this.compare(cx))),
            );
        #[cfg(not(target_arch = "wasm32"))]
        let dialog = dialog.button(
            Button::new("sc-deploy")
                .primary()
                .label(crate::localization::text(
                    "explorer.schema_compare.deploy_button",
                ))
                .disabled(self.differences.is_empty() || target_is_snapshot)
                .on_click(cx.listener(|this, _, _window, cx| this.deploy(cx))),
        );
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_rows_map_to_connections_then_snapshots() {
        assert_eq!(picker_entry(2, 3, Some(0)), Some(PickerEntry::Profile(0)));
        assert_eq!(picker_entry(2, 3, Some(1)), Some(PickerEntry::Profile(1)));
        assert_eq!(picker_entry(2, 3, Some(2)), Some(PickerEntry::Snapshot(0)));
        assert_eq!(picker_entry(2, 3, Some(4)), Some(PickerEntry::Snapshot(2)));
        assert_eq!(picker_entry(2, 3, Some(5)), None);
        assert_eq!(picker_entry(2, 3, None), None);
    }

    fn obj(schema: &str, name: &str, kind: &str, def: &str) -> SchemaObject {
        SchemaObject {
            schema: schema.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            definition: def.to_string(),
        }
    }

    #[test]
    fn collect_sql_covers_indexes_fks_and_constraints() {
        let sql = collect_objects_sql(DatabaseType::SqlServer, "Sales").unwrap();
        assert!(sql.contains("sys.indexes"), "{sql}");
        assert!(sql.contains("is_included_column = 1"), "{sql}");
        assert!(sql.contains("sys.foreign_keys"), "{sql}");
        assert!(sql.contains("delete_referential_action_desc"), "{sql}");
        assert!(sql.contains("sys.check_constraints"), "{sql}");
        assert!(sql.contains("sys.default_constraints"), "{sql}");
        // Signature kinds render as comments so the generated script stays
        // inert for them.
        assert!(sql.contains("CONCAT('-- ',"), "{sql}");
    }

    #[test]
    fn signature_kinds_diff_like_any_object() {
        let a = obj(
            "dbo",
            "Orders.IX_Orders_Date",
            "INDEX",
            "-- nonclustered (OrderDate)",
        );
        let b = obj(
            "dbo",
            "Orders.IX_Orders_Date",
            "INDEX",
            "-- unique nonclustered (OrderDate)",
        );
        let diffs = compare_objects(&[a], &[b], DatabaseType::SqlServer, DatabaseType::SqlServer);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].status, DifferenceStatus::Changed);
    }

    #[test]
    fn collect_sql_covers_modules_and_tables() {
        let sql = collect_objects_sql(DatabaseType::SqlServer, "Sales").unwrap();
        assert!(sql.starts_with("USE [Sales];"));
        assert!(sql.contains("sys.sql_modules"));
        assert!(sql.contains("sys.tables"));
        assert!(sql.contains("STRING_AGG"));
    }

    /// Regression (Msg 451): the `*_desc` columns of the system views carry
    /// the fixed catalog collation (Latin1_General_CI_AS_KS_WS) while object
    /// names carry the database's. CONCAT-ing the two fails on a default
    /// SQL_Latin1_General_CP1_CI_AS database, so every `_desc` read is pinned
    /// to the database collation.
    #[test]
    fn collect_sql_pins_catalog_desc_columns_to_the_database_collation() {
        let sql = collect_objects_sql(DatabaseType::SqlServer, "Sales").unwrap();
        let mut seen = 0;
        for (at, _) in sql.match_indices("_desc") {
            let rest = &sql[at + "_desc".len()..];
            assert!(
                rest.starts_with(" COLLATE DATABASE_DEFAULT"),
                "unpinned catalog column near: {}",
                &sql[at.saturating_sub(40)..(at + 40).min(sql.len())]
            );
            seen += 1;
        }
        assert_eq!(seen, 3, "type_desc + both referential action descs");
    }

    /// Live check against a real SQL Server (skipped unless
    /// `.sqlly-local-sql.json` or `SQLLY_SQL_*` is configured): the whole
    /// metadata batch must run without a collation conflict.
    #[test]
    fn collect_sql_runs_clean_on_a_live_sql_server() {
        let Some(cfg) = sqlly_testkit::LiveSqlConfig::load() else {
            return;
        };
        crate::ensure_test_tokio_runtime();
        let profile = cfg.to_profile("schema-compare-live");
        let sql = collect_objects_sql(DatabaseType::SqlServer, &cfg.database).unwrap();
        let services = sqlly_services::Services::new();
        let outcome = crate::tokio_handle().block_on(crate::tool_dialog::execute_collect(
            &services, &profile, &sql,
        ));
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.sets.len(), 6, "one set per object family");
    }

    #[test]
    fn collect_sql_escapes_database_brackets() {
        assert!(
            collect_objects_sql(DatabaseType::SqlServer, "we]ird")
                .unwrap()
                .starts_with("USE [we]]ird];")
        );
    }

    #[test]
    fn native_collectors_cover_each_supported_engine_family() {
        let postgres = collect_objects_sql(DatabaseType::Postgres, "db").unwrap();
        assert!(postgres.contains("pg_get_functiondef"));
        assert!(postgres.contains("pg_get_constraintdef"));
        let mysql = collect_objects_sql(DatabaseType::MySql, "db").unwrap();
        assert!(mysql.contains("information_schema.statistics"));
        assert!(mysql.contains("information_schema.triggers"));
        let sqlite = collect_objects_sql(DatabaseType::Sqlite, "db").unwrap();
        assert!(sqlite.contains("sqlite_master"));
        assert!(sqlite.contains("pragma_foreign_key_list"));
        let duckdb = collect_objects_sql(DatabaseType::DuckDb, "db").unwrap();
        for catalog in [
            "duckdb_tables()",
            "duckdb_views()",
            "duckdb_indexes()",
            "duckdb_constraints()",
            "duckdb_sequences()",
            "duckdb_functions()",
        ] {
            assert!(duckdb.contains(catalog), "{catalog}: {duckdb}");
        }
        let oracle = collect_objects_sql(DatabaseType::Oracle, "db").unwrap();
        assert!(oracle.contains("DBMS_METADATA.SET_TRANSFORM_PARAM"));
        assert!(oracle.contains("DBMS_METADATA.GET_DDL('TABLE'"));
        assert!(oracle.contains("all_constraints"));
        assert!(collect_objects_sql(DatabaseType::Db2, "db").is_err());
    }

    /// Runs the DuckDB collector against a real database file and shapes
    /// the rows the way `objects_from_outcome` does.
    #[cfg(not(target_arch = "wasm32"))]
    async fn collect_duckdb(profile: &ConnectionProfile) -> Vec<SchemaObject> {
        let client = sqlly_engines::duckdb::open_client(profile).await.unwrap();
        let batch = collect_objects_sql(DatabaseType::DuckDb, "db").unwrap();
        let mut objects = Vec::new();
        for statement in
            sqlly_lang::sqllex::split_statements(&batch, sqlly_lang::sqlmodel::Dialect::Postgres)
        {
            if statement.text.trim().is_empty() {
                continue;
            }
            let sets = sqlly_engines::duckdb::query_rows_on_client(&client, statement.text, &[])
                .await
                .unwrap();
            for set in sets {
                for row in set.rows {
                    let text = |i: usize| match row.get(i) {
                        Some(CellValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    objects.push(obj(&text(0), &text(1), &text(2), &text(3)));
                }
            }
        }
        objects
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn duckdb_collector_reads_a_real_catalog_and_diffs_two_databases() {
        use sqlly_core::{SecretProvider, SecretRef};
        let dir = tempfile::tempdir().unwrap();
        let profile = |name: &str| {
            let mut p = ConnectionProfile::sql_login(
                name,
                dir.path().join(name).to_string_lossy().as_ref(),
                "",
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: String::new(),
                },
            );
            p.database_type = DatabaseType::DuckDb;
            p
        };
        let source = profile("source.duckdb");
        let target = profile("target.duckdb");
        let seed = |profile: ConnectionProfile, sql: &'static str| async move {
            let client = sqlly_engines::duckdb::open_client(&profile).await.unwrap();
            sqlly_engines::duckdb::execute_batch_on_client(&client, sql)
                .await
                .unwrap();
        };
        seed(
            source.clone(),
            "CREATE SEQUENCE order_id;\
             CREATE TABLE customer (id INTEGER PRIMARY KEY, name VARCHAR UNIQUE);\
             CREATE TABLE orders (id INTEGER DEFAULT nextval('order_id') PRIMARY KEY, \
             customer_id INTEGER REFERENCES customer(id), amount DECIMAL(10,2) CHECK (amount > 0));\
             CREATE INDEX ix_orders_customer ON orders(customer_id);\
             CREATE VIEW v_orders AS SELECT id, amount FROM orders;\
             CREATE MACRO add_tax(x) AS x * 1.2;",
        )
        .await;
        seed(
            target.clone(),
            "CREATE TABLE customer (id INTEGER PRIMARY KEY, name VARCHAR UNIQUE);\
             CREATE VIEW v_orders AS SELECT id FROM customer;\
             CREATE TABLE legacy (id INTEGER);",
        )
        .await;

        let source_objects = collect_duckdb(&source).await;
        let kinds = |kind: &str| {
            source_objects
                .iter()
                .filter(|o| o.kind == kind)
                .map(|o| o.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds("TABLE"), vec!["customer", "orders"]);
        assert_eq!(kinds("VIEW"), vec!["v_orders"]);
        assert_eq!(kinds("INDEX"), vec!["orders.ix_orders_customer"]);
        assert_eq!(kinds("SEQUENCE"), vec!["order_id"]);
        assert_eq!(kinds("ROUTINE"), vec!["add_tax"]);
        assert_eq!(kinds("FOREIGN KEY").len(), 1);
        assert_eq!(kinds("PRIMARY KEY").len(), 2);
        assert_eq!(kinds("UNIQUE CONSTRAINT").len(), 1);
        assert_eq!(kinds("CHECK CONSTRAINT").len(), 1);
        assert!(source_objects.iter().all(|o| o.schema == "main"));
        let table = source_objects
            .iter()
            .find(|o| o.kind == "TABLE" && o.name == "orders")
            .unwrap();
        assert!(
            table.definition.starts_with("CREATE TABLE orders("),
            "{}",
            table.definition
        );
        let fk = source_objects
            .iter()
            .find(|o| o.kind == "FOREIGN KEY")
            .unwrap();
        assert!(
            fk.definition.starts_with("-- FOREIGN KEY"),
            "{}",
            fk.definition
        );
        let sequence = source_objects
            .iter()
            .find(|o| o.kind == "SEQUENCE")
            .unwrap();
        assert_eq!(
            sequence.definition,
            "CREATE SEQUENCE order_id INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START 1 NO CYCLE;"
        );

        let target_objects = collect_duckdb(&target).await;
        let diffs = compare_objects(
            &source_objects,
            &target_objects,
            DatabaseType::DuckDb,
            DatabaseType::DuckDb,
        );
        let status_of = |kind: &str, name: &str| {
            diffs
                .iter()
                .find(|d| d.object.kind == kind && d.object.name == name)
                .map(|d| d.status.clone())
                .unwrap_or_else(|| panic!("{kind} {name} missing from {diffs:?}"))
        };
        assert_eq!(
            status_of("TABLE", "orders"),
            DifferenceStatus::MissingInTarget
        );
        assert_eq!(
            status_of("TABLE", "legacy"),
            DifferenceStatus::ExtraInTarget
        );
        assert_eq!(status_of("VIEW", "v_orders"), DifferenceStatus::Changed);
        assert_eq!(
            status_of("SEQUENCE", "order_id"),
            DifferenceStatus::MissingInTarget
        );
        assert_eq!(
            status_of("ROUTINE", "add_tax"),
            DifferenceStatus::MissingInTarget
        );
        assert!(
            !diffs
                .iter()
                .any(|d| d.object.name == "customer" && d.object.kind == "TABLE")
        );

        // The deploy plan runs DuckDB's own DDL: the stored CREATE statements
        // verbatim, the view replaced in place.
        let plan = plan_deploy(
            &diffs,
            DatabaseType::DuckDb,
            DatabaseType::DuckDb,
            DeployOptions::default(),
        );
        let step = |identity: &str| {
            plan.steps
                .iter()
                .find(|s| s.identity == identity)
                .unwrap_or_else(|| panic!("{identity} missing from {:?}", plan.steps))
        };
        assert!(step("main.orders").runnable);
        assert!(step("main.orders").sql.starts_with("CREATE TABLE orders("));
        assert!(step("main.order_id").runnable);
        assert!(
            step("main.add_tax")
                .sql
                .starts_with("CREATE MACRO add_tax(x) AS")
        );
        assert!(
            !step("main.legacy").runnable,
            "target-only objects are never dropped"
        );
        assert_eq!(
            step("main.v_orders").sql,
            "CREATE OR REPLACE VIEW v_orders AS SELECT id, amount FROM orders;"
        );
        let deploy = deploy_script(&plan, "");
        let client = sqlly_engines::duckdb::open_client(&target).await.unwrap();
        sqlly_engines::duckdb::execute_batch_on_client(&client, &deploy)
            .await
            .unwrap();
        let after = collect_duckdb(&target).await;
        let remaining = compare_objects(
            &source_objects,
            &after,
            DatabaseType::DuckDb,
            DatabaseType::DuckDb,
        );
        assert!(
            remaining
                .iter()
                .all(|d| d.status == DifferenceStatus::ExtraInTarget),
            "{remaining:?}"
        );
    }

    #[test]
    fn objects_from_outcome_reads_both_sets() {
        let outcome = ToolQueryOutcome {
            sets: vec![
                ToolQuerySet {
                    columns: Vec::new(),
                    rows: vec![vec![
                        CellValue::String("dbo".into()),
                        CellValue::String("v1".into()),
                        CellValue::String("VIEW".into()),
                        CellValue::String("CREATE VIEW v1 AS SELECT 1".into()),
                    ]],
                },
                ToolQuerySet {
                    columns: Vec::new(),
                    rows: vec![vec![
                        CellValue::String("dbo".into()),
                        CellValue::String("t1".into()),
                        CellValue::String("TABLE".into()),
                        CellValue::String("id int(4) NOT NULL".into()),
                    ]],
                },
            ],
            error: None,
        };
        let objects = objects_from_outcome(&outcome);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[1].kind, "TABLE");
    }

    #[test]
    fn capped_metadata_is_never_treated_as_a_complete_catalog() {
        let outcome = ToolQueryOutcome {
            sets: vec![ToolQuerySet {
                columns: Vec::new(),
                rows: vec![Vec::new(); TOOL_QUERY_ROW_CAP],
            }],
            error: None,
        };
        assert!(collection_limit_reached(&outcome));
    }

    #[test]
    fn diff_grid_rows_describe_the_change() {
        let source = vec![obj("dbo", "P1", "PROC", "CREATE PROC P1 AS SELECT 1")];
        let target = vec![obj("dbo", "P1", "PROC", "CREATE PROC P1 AS SELECT 2")];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let risks = diffs.iter().map(base_risk).collect::<Vec<_>>();
        let set = diff_grid_set(&diffs, &risks);
        assert_eq!(set.rows.len(), 1);
        assert_eq!(
            set.rows[0][0],
            CellValue::String("Definition changed".into())
        );
        assert_eq!(set.rows[0][2], CellValue::String("dbo.P1".into()));
    }

    #[test]
    fn risk_column_present() {
        let source = vec![obj("dbo", "P1", "PROC", "CREATE PROC P1 AS SELECT 1")];
        let target = vec![
            obj("dbo", "P1", "PROC", "CREATE PROC P1 AS SELECT 2"),
            obj("dbo", "Old", "TABLE", "id int NOT NULL"),
        ];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let risks = diffs.iter().map(base_risk).collect::<Vec<_>>();
        let set = diff_grid_set(&diffs, &risks);
        assert_eq!(set.columns.len(), 5);
        assert_eq!(set.columns[4].name, "Risk");
        for (row, risk) in set.rows.iter().zip(&risks) {
            assert_eq!(row[4], CellValue::String(risk.label().to_string()));
        }
        assert!(
            set.rows
                .iter()
                .any(|row| row[4] == CellValue::String("Data loss".into())),
            "the target-only table row is marked as data loss"
        );
    }

    #[test]
    fn sorted_data_loss_first() {
        // Alphabetically the changed proc and the target-only view sort
        // before the target-only table; risk rank must win over object order.
        let diffs = vec![
            ComparedDifference {
                status: DifferenceStatus::Changed,
                object: obj("dbo", "AProc", "PROC", "CREATE PROC AProc AS SELECT 1"),
                target_definition: Some("CREATE PROC AProc AS SELECT 2".into()),
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj("dbo", "BView", "VIEW", "CREATE VIEW BView AS SELECT 1"),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::MissingInTarget,
                object: obj("dbo", "ATable", "TABLE", "id int NOT NULL"),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj("dbo", "ZTable", "TABLE", "id int NOT NULL"),
                target_definition: None,
                note: String::new(),
            },
        ];
        let risks = diffs.iter().map(base_risk).collect::<Vec<_>>();
        let order = crate::schema_compare_risk::display_order(&risks, &diffs);
        assert_eq!(order[0], 3, "the target-only table (data loss) sorts first");
        assert_eq!(order[1], 1, "the target-only view (breaking) is second");
        assert_eq!(order[2], 0, "the unassessed change is third");
        assert_eq!(order[3], 2, "the additive difference (safe) is last");
    }

    #[test]
    fn deploy_input_unchanged() {
        let source = vec![
            obj("dbo", "P1", "PROC", "CREATE PROC P1 AS SELECT 1"),
            obj("dbo", "New", "TABLE", "id int NOT NULL"),
        ];
        let target = vec![
            obj("dbo", "P1", "PROC", "CREATE PROC P1 AS SELECT 2"),
            obj("dbo", "Old", "TABLE", "id int NOT NULL"),
        ];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let before = diffs.clone();
        let options = DeployOptions {
            include_drops: true,
            ..DeployOptions::default()
        };
        let baseline_script = deploy_script(
            &plan_deploy(
                &before,
                DatabaseType::SqlServer,
                DatabaseType::SqlServer,
                options,
            ),
            "-- header\n",
        );
        // Run the whole risk pipeline over the differences the way the pane
        // does: base risks, display order, and the ordered grid view.
        let risks = diffs.iter().map(base_risk).collect::<Vec<_>>();
        let order = crate::schema_compare_risk::display_order(&risks, &diffs);
        let ordered = order
            .iter()
            .map(|&index| diffs[index].clone())
            .collect::<Vec<_>>();
        let ordered_risks = order.iter().map(|&index| risks[index]).collect::<Vec<_>>();
        let _ = diff_grid_set(&ordered, &ordered_risks);
        // The vector deploy reads is untouched, and the script it produces is
        // byte-for-byte the same.
        assert_eq!(diffs, before);
        let script = deploy_script(
            &plan_deploy(
                &diffs,
                DatabaseType::SqlServer,
                DatabaseType::SqlServer,
                options,
            ),
            "-- header\n",
        );
        assert_eq!(script, baseline_script);
    }

    #[test]
    fn rename_toggle_rebuilds_differences_from_raw() {
        let source = vec![obj("dbo", "customers", "TABLE", "id int")];
        let target = vec![obj("dbo", "customer", "TABLE", "id int")];
        let raw = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let candidates = detect_renames(&raw)
            .into_iter()
            .map(|candidate| (candidate, true))
            .collect::<Vec<_>>();
        let rebuilt = rebuild_differences_from_raw(&raw, &candidates);
        assert_eq!(rebuilt.len(), 1);
        assert!(matches!(
            rebuilt[0].status,
            DifferenceStatus::Renamed { .. }
        ));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn diff_rows_mark_added_and_removed_lines() {
        let rows = diff_grid_rows("old line\nsame", "same\nnew line");
        assert!(
            rows.iter()
                .any(|(source, _, change)| change == "+" && source == "new line")
        );
        assert!(
            rows.iter()
                .any(|(_, target, change)| change == "-" && target == "old line")
        );
    }
}
