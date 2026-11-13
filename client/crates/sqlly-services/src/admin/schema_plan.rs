//! Engine-aware schema comparison, rename detection, and dependency-ordered
//! deploy/rollback script planning shared by the GPUI pane and services layer.

use sqlly_lang::sqllex::StrictLexer;
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, Dialect};

use crate::admin::advanced_tools::SchemaObject;

/// The Oracle metadata batch behind Schema Compare and Schema History: one
/// PL/SQL block that configures `DBMS_METADATA` for the session (no storage,
/// segment or tablespace clauses, no owner prefix, no `;` terminators, so two
/// databases with the same logical schema render identical DDL), then one
/// `SELECT` per object kind, each returning the four text columns every
/// collector shares: schema, name, kind, definition. Everything is scoped to
/// the session's current schema.
///
/// Tables, views, materialized views, routines (procedures, functions,
/// package specs and bodies), types, triggers, sequences and synonyms come
/// from `DBMS_METADATA.GET_DDL`; indexes too, minus the ones that back a
/// primary-key or unique constraint. Constraints are rendered as runnable
/// `ALTER TABLE … ADD …` statements built from `ALL_CONSTRAINTS` /
/// `ALL_CONS_COLUMNS`; a system-generated constraint name is replaced by a
/// stable `(PRIMARY KEY "ID")`-style key so the same constraint matches
/// across databases. Table DDL keeps its inline constraints (turning them
/// off would also drop `NOT NULL`) but not its foreign keys, which are listed
/// separately so they deploy after every table. Sequences are normalized to
/// `START WITH 1` because the live value drifts with use. Each `GET_DDL`
/// runs over a `NO_MERGE` inline view so the owner filter is applied before
/// the function is called — merged, the optimizer renders every table in
/// the database first. Setting the session transform makes the next table
/// render re-parse DBMS_METADATA's stylesheet (many seconds), so the block
/// marks the session (`CLIENT_INFO = SQLLY_DDL_V1`) and is skipped on a
/// session that already carries the transforms. Needs Oracle 12.2+
/// (`SEARCH_CONDITION_VC`).
pub fn oracle_collect_objects_sql() -> String {
    const CUR: &str = "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')";
    let cols = |owner: &str, constraint: &str| {
        format!(
            "(SELECT LISTAGG('\"' || cc.column_name || '\"', ', ') WITHIN GROUP (ORDER BY cc.position) FROM all_cons_columns cc WHERE cc.owner = {owner} AND cc.constraint_name = {constraint})"
        )
    };
    // `TRIM` only strips spaces; GET_DDL output opens with a newline and
    // two spaces, so both ends lose every whitespace character.
    let ddl = |expr: &str| {
        format!(
            "LTRIM(RTRIM({expr}, ' ' || CHR(9) || CHR(10) || CHR(13)), ' ' || CHR(9) || CHR(10) || CHR(13))"
        )
    };
    let own_cols = cols("k.owner", "k.constraint_name");
    let ref_cols = cols("k.r_owner", "k.r_constraint_name");
    let table_ddl = ddl("DBMS_METADATA.GET_DDL('TABLE', t.table_name, t.owner)");
    let view_ddl = ddl("DBMS_METADATA.GET_DDL('VIEW', v.view_name, v.owner)");
    let mview_ddl = ddl("DBMS_METADATA.GET_DDL('MATERIALIZED_VIEW', m.mview_name, m.owner)");
    let index_ddl = ddl("DBMS_METADATA.GET_DDL('INDEX', i.index_name, i.owner)");
    let routine_ddl = ddl(
        "DBMS_METADATA.GET_DDL(CASE o.object_type WHEN 'PACKAGE' THEN 'PACKAGE_SPEC' WHEN 'PACKAGE BODY' THEN 'PACKAGE_BODY' WHEN 'TYPE' THEN 'TYPE_SPEC' WHEN 'TYPE BODY' THEN 'TYPE_BODY' ELSE o.object_type END, o.object_name, o.owner)",
    );
    let trigger_ddl = ddl("DBMS_METADATA.GET_DDL('TRIGGER', t.trigger_name, t.owner)");
    let sequence_ddl = ddl("DBMS_METADATA.GET_DDL('SEQUENCE', s.sequence_name, s.sequence_owner)");
    let synonym_ddl = ddl("DBMS_METADATA.GET_DDL('SYNONYM', s.synonym_name, s.owner)");
    let statements = [
        "BEGIN \
         IF NVL(SYS_CONTEXT('USERENV', 'CLIENT_INFO'), ' ') <> 'SQLLY_DDL_V1' THEN \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'TABLESPACE', FALSE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', FALSE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'EMIT_SCHEMA', FALSE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'CONSTRAINTS', TRUE); \
         DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'REF_CONSTRAINTS', FALSE); \
         DBMS_APPLICATION_INFO.SET_CLIENT_INFO('SQLLY_DDL_V1'); \
         END IF; \
         END;"
            .to_string(),
        format!(
            "SELECT t.owner AS schema_name, t.table_name AS object_name, 'TABLE' AS kind, {table_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ t.owner, t.table_name FROM all_tables t WHERE t.owner = {CUR} AND t.table_name NOT LIKE 'BIN$%' AND t.nested = 'NO' AND t.secondary = 'N' AND NVL(t.iot_type, 'X') <> 'IOT_OVERFLOW' AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name)) t ORDER BY t.table_name"
        ),
        format!(
            "SELECT v.owner AS schema_name, v.view_name AS object_name, 'VIEW' AS kind, {view_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ v.owner, v.view_name FROM all_views v WHERE v.owner = {CUR}) v ORDER BY v.view_name"
        ),
        format!(
            "SELECT m.owner AS schema_name, m.mview_name AS object_name, 'MATERIALIZED VIEW' AS kind, {mview_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ m.owner, m.mview_name FROM all_mviews m WHERE m.owner = {CUR}) m ORDER BY m.mview_name"
        ),
        format!(
            "SELECT i.table_owner AS schema_name, i.table_name || '.' || i.index_name AS object_name, 'INDEX' AS kind, {index_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ i.owner, i.table_owner, i.table_name, i.index_name FROM all_indexes i WHERE i.owner = {CUR} AND i.table_owner = {CUR} AND i.index_type <> 'LOB' AND i.generated = 'N' AND i.table_name NOT LIKE 'BIN$%' AND NOT EXISTS (SELECT 1 FROM all_constraints k WHERE k.owner = i.table_owner AND k.table_name = i.table_name AND k.index_name = i.index_name AND k.constraint_type IN ('P', 'U'))) i ORDER BY i.table_name, i.index_name"
        ),
        format!(
            "SELECT c.owner AS schema_name, c.table_name || '.' || CASE WHEN c.generated = 'USER NAME' THEN c.constraint_name ELSE '(' || CASE c.constraint_type WHEN 'P' THEN 'PRIMARY KEY' WHEN 'U' THEN 'UNIQUE' ELSE 'CHECK' END || ' ' || NVL(c.cols, c.search_condition_vc) || ')' END AS object_name, CASE c.constraint_type WHEN 'P' THEN 'PRIMARY KEY' WHEN 'U' THEN 'UNIQUE CONSTRAINT' ELSE 'CHECK CONSTRAINT' END AS kind, 'ALTER TABLE \"' || c.table_name || '\" ADD ' || CASE WHEN c.generated = 'USER NAME' THEN 'CONSTRAINT \"' || c.constraint_name || '\" ' ELSE '' END || CASE c.constraint_type WHEN 'P' THEN 'PRIMARY KEY (' || c.cols || ')' WHEN 'U' THEN 'UNIQUE (' || c.cols || ')' ELSE 'CHECK (' || c.search_condition_vc || ')' END AS definition FROM (SELECT k.owner, k.table_name, k.constraint_name, k.constraint_type, k.generated, k.search_condition_vc, {own_cols} AS cols FROM all_constraints k WHERE k.owner = {CUR} AND k.constraint_type IN ('P', 'U', 'C') AND k.table_name NOT LIKE 'BIN$%' AND (k.constraint_type <> 'C' OR k.generated = 'USER NAME')) c ORDER BY object_name"
        ),
        format!(
            "SELECT c.owner AS schema_name, c.table_name || '.' || CASE WHEN c.generated = 'USER NAME' THEN c.constraint_name ELSE '(FOREIGN KEY ' || c.cols || ')' END AS object_name, 'FOREIGN KEY' AS kind, 'ALTER TABLE \"' || c.table_name || '\" ADD ' || CASE WHEN c.generated = 'USER NAME' THEN 'CONSTRAINT \"' || c.constraint_name || '\" ' ELSE '' END || 'FOREIGN KEY (' || c.cols || ') REFERENCES ' || CASE WHEN c.r_owner = c.owner THEN '' ELSE '\"' || c.r_owner || '\".' END || '\"' || c.r_table || '\" (' || c.rcols || ')' || CASE c.delete_rule WHEN 'CASCADE' THEN ' ON DELETE CASCADE' WHEN 'SET NULL' THEN ' ON DELETE SET NULL' ELSE '' END AS definition FROM (SELECT k.owner, k.table_name, k.constraint_name, k.generated, k.r_owner, k.delete_rule, (SELECT r.table_name FROM all_constraints r WHERE r.owner = k.r_owner AND r.constraint_name = k.r_constraint_name) AS r_table, {own_cols} AS cols, {ref_cols} AS rcols FROM all_constraints k WHERE k.owner = {CUR} AND k.constraint_type = 'R' AND k.table_name NOT LIKE 'BIN$%') c ORDER BY object_name"
        ),
        format!(
            "SELECT o.owner AS schema_name, o.object_name || CASE WHEN o.object_type IN ('PACKAGE BODY', 'TYPE BODY') THEN ' (BODY)' ELSE '' END AS object_name, CASE WHEN o.object_type IN ('TYPE', 'TYPE BODY') THEN 'TYPE' ELSE 'ROUTINE' END AS kind, {routine_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ o.owner, o.object_name, o.object_type FROM all_objects o WHERE o.owner = {CUR} AND o.object_type IN ('PROCEDURE', 'FUNCTION', 'PACKAGE', 'PACKAGE BODY', 'TYPE', 'TYPE BODY') AND o.generated = 'N' AND o.object_name NOT LIKE 'SYS\\_%' ESCAPE '\\') o ORDER BY o.object_name, o.object_type"
        ),
        format!(
            "SELECT t.owner AS schema_name, NVL(t.table_name, '(schema)') || '.' || t.trigger_name AS object_name, 'TRIGGER' AS kind, {trigger_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ t.owner, t.table_name, t.trigger_name FROM all_triggers t WHERE t.owner = {CUR}) t ORDER BY object_name"
        ),
        format!(
            "SELECT s.sequence_owner AS schema_name, s.sequence_name AS object_name, 'SEQUENCE' AS kind, REGEXP_REPLACE({sequence_ddl}, 'START WITH [0-9]+', 'START WITH 1') AS definition FROM (SELECT /*+ NO_MERGE */ s.sequence_owner, s.sequence_name FROM all_sequences s WHERE s.sequence_owner = {CUR} AND s.sequence_name NOT LIKE 'ISEQ$$%') s ORDER BY s.sequence_name"
        ),
        format!(
            "SELECT s.owner AS schema_name, s.synonym_name AS object_name, 'SYNONYM' AS kind, {synonym_ddl} AS definition FROM (SELECT /*+ NO_MERGE */ s.owner, s.synonym_name FROM all_synonyms s WHERE s.owner = {CUR}) s ORDER BY s.synonym_name"
        ),
    ];
    statements.join(";\n")
}

/// The object kinds [`oracle_collect_objects_sql`] emits, in the order its
/// statements run. The first nine are the shared plan vocabulary; the last
/// four exist only on Oracle.
pub const ORACLE_OBJECT_KINDS: [&str; 13] = [
    "TABLE",
    "VIEW",
    "MATERIALIZED VIEW",
    "INDEX",
    "PRIMARY KEY",
    "UNIQUE CONSTRAINT",
    "CHECK CONSTRAINT",
    "FOREIGN KEY",
    "ROUTINE",
    "TYPE",
    "TRIGGER",
    "SEQUENCE",
    "SYNONYM",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DifferenceStatus {
    MissingInTarget,
    ExtraInTarget,
    Changed,
    Unsupported,
    ManualReview,
    Renamed { from: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComparedDifference {
    pub status: DifferenceStatus,
    pub object: SchemaObject,
    pub target_definition: Option<String>,
    pub note: String,
}

impl ComparedDifference {
    pub fn note_message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self.note.as_str() {
            "Definitions use different engine dialects; translation may be lossy and requires manual review." => {
                DisplayMessage::new(
                    "services.schema_compare.notes.cross_engine",
                    self.note.clone(),
                )
            }
            "The normalized definitions differ." => {
                DisplayMessage::new("services.schema_compare.notes.changed", self.note.clone())
            }
            "The object is absent from the target metadata." => DisplayMessage::new(
                "services.schema_compare.notes.missing_target",
                self.note.clone(),
            ),
            "The target-only object is reported but never scripted for deletion." => {
                DisplayMessage::new(
                    "services.schema_compare.notes.extra_target",
                    self.note.clone(),
                )
            }
            "Rename accepted by the user." => DisplayMessage::new(
                "services.schema_compare.notes.rename_accepted",
                self.note.clone(),
            ),
            _ if self.note.starts_with("The target-only") => DisplayMessage::raw(self.note.clone()),
            _ if self.note.contains("does not expose a comparable") => DisplayMessage::new(
                "services.schema_compare.notes.unsupported",
                self.note.clone(),
            )
            .with_arg("object_type", self.object.kind.clone())
            .with_arg(
                "engine_name",
                self.note
                    .split(" does not expose")
                    .next()
                    .unwrap_or_default(),
            ),
            _ => DisplayMessage::raw(self.note.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameCandidate {
    pub kind: String,
    pub schema: String,
    pub from_name: String,
    pub to_name: String,
    pub similarity_percent: u8,
}

pub const RENAME_SIMILARITY_THRESHOLD: u8 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeployOptions {
    pub include_drops: bool,
    pub wrap_in_transaction: bool,
    /// Major version of the PostgreSQL target server, when known. PostgreSQL
    /// 14 added `CREATE OR REPLACE TRIGGER`, so a trigger change is scripted
    /// in place only when this is 14 or later; `None` means unknown.
    pub target_postgres_major: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployStep {
    pub identity: String,
    pub kind: String,
    pub status: DifferenceStatus,
    pub definition: String,
    pub sql: String,
    pub rollback_sql: String,
    pub runnable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DeployPlan {
    pub steps: Vec<DeployStep>,
    pub warnings: Vec<sqlly_core::DisplayMessage>,
}

pub fn object_key(object: &SchemaObject) -> (&str, &str, &str) {
    (&object.kind, &object.schema, &object.name)
}

pub fn comparable_definition(definition: &str) -> String {
    definition.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn supports_kind(database_type: DatabaseType, kind: &str) -> bool {
    match database_type.connection_kind() {
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => !matches!(
            kind,
            "ROUTINE"
                | "PRIMARY KEY"
                | "UNIQUE CONSTRAINT"
                | "CHECK CONSTRAINT"
                | "DEFAULT CONSTRAINT"
        ),
        ConnectionKind::PostgresWire => kind != "DEFAULT CONSTRAINT",
        ConnectionKind::MySqlWire => kind != "DEFAULT CONSTRAINT",
        ConnectionKind::Tds => true,
        // DuckDB has tables, views, indexes, sequences, macros (its
        // routines) and table constraints, but no triggers and no named
        // default constraints.
        ConnectionKind::DuckDbFile => !matches!(kind, "TRIGGER" | "DEFAULT CONSTRAINT"),
        // ClickHouse: no routines, triggers, foreign keys or unique/default
        // constraints; tables, views, materialized views, dictionaries,
        // data-skipping indexes, primary keys and CHECK constraints compare.
        ConnectionKind::ClickHouseHttp => !matches!(
            kind,
            "ROUTINE" | "TRIGGER" | "FOREIGN KEY" | "UNIQUE CONSTRAINT" | "DEFAULT CONSTRAINT"
        ),
        // Oracle has no default constraints (defaults live on the column);
        // every other kind, including its own sequences, synonyms, types
        // and materialized views, is collected.
        ConnectionKind::OracleNet => kind != "DEFAULT CONSTRAINT",
        _ => false,
    }
}

pub fn compare_objects(
    source: &[SchemaObject],
    target: &[SchemaObject],
    source_type: DatabaseType,
    target_type: DatabaseType,
) -> Vec<ComparedDifference> {
    let mut differences = Vec::new();
    for source_object in source {
        match target
            .iter()
            .find(|candidate| object_key(candidate) == object_key(source_object))
        {
            Some(target_object)
                if comparable_definition(&target_object.definition)
                    != comparable_definition(&source_object.definition) =>
            {
                let cross_engine = source_type.connection_kind() != target_type.connection_kind();
                differences.push(ComparedDifference {
                    status: if cross_engine {
                        DifferenceStatus::ManualReview
                    } else {
                        DifferenceStatus::Changed
                    },
                    object: source_object.clone(),
                    target_definition: Some(target_object.definition.clone()),
                    note: if cross_engine {
                        "Definitions use different engine dialects; translation may be lossy and requires manual review.".to_string()
                    } else {
                        "The normalized definitions differ.".to_string()
                    },
                });
            }
            Some(_) => {}
            None => differences.push(ComparedDifference {
                status: if supports_kind(target_type, &source_object.kind) {
                    DifferenceStatus::MissingInTarget
                } else {
                    DifferenceStatus::Unsupported
                },
                object: source_object.clone(),
                target_definition: None,
                note: if supports_kind(target_type, &source_object.kind) {
                    "The object is absent from the target metadata.".to_string()
                } else {
                    format!(
                        "{} does not expose a comparable {} object.",
                        target_type.display_name(),
                        source_object.kind
                    )
                },
            }),
        }
    }
    for target_object in target {
        if source
            .iter()
            .any(|candidate| object_key(candidate) == object_key(target_object))
        {
            continue;
        }
        differences.push(ComparedDifference {
            status: if supports_kind(source_type, &target_object.kind) {
                DifferenceStatus::ExtraInTarget
            } else {
                DifferenceStatus::Unsupported
            },
            object: target_object.clone(),
            target_definition: None,
            note: if supports_kind(source_type, &target_object.kind) {
                "The target-only object is reported but never scripted for deletion.".to_string()
            } else {
                format!(
                    "{} does not expose a comparable {} object.",
                    source_type.display_name(),
                    target_object.kind
                )
            },
        });
    }
    differences.sort_by(|left, right| object_key(&left.object).cmp(&object_key(&right.object)));
    differences
}

fn signature_tokens(definition: &str) -> Vec<String> {
    comparable_definition(definition)
        .split(", ")
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn dice_similarity(left: &[String], right: &[String]) -> u8 {
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let intersection = left.iter().filter(|token| right.contains(token)).count();
    ((2 * intersection * 100) / (left.len() + right.len())) as u8
}

pub fn detect_renames(differences: &[ComparedDifference]) -> Vec<RenameCandidate> {
    let missing: Vec<&ComparedDifference> = differences
        .iter()
        .filter(|d| d.status == DifferenceStatus::MissingInTarget)
        .collect();
    let extra: Vec<&ComparedDifference> = differences
        .iter()
        .filter(|d| d.status == DifferenceStatus::ExtraInTarget)
        .collect();
    let mut candidates = Vec::new();
    for source in &missing {
        if source.object.definition.trim().is_empty() {
            continue;
        }
        let source_tokens = signature_tokens(&source.object.definition);
        for target in &extra {
            if source.object.kind != target.object.kind
                || source.object.schema != target.object.schema
                || target.object.definition.trim().is_empty()
            {
                continue;
            }
            let similarity =
                dice_similarity(&source_tokens, &signature_tokens(&target.object.definition));
            if similarity >= RENAME_SIMILARITY_THRESHOLD {
                candidates.push(RenameCandidate {
                    kind: source.object.kind.clone(),
                    schema: source.object.schema.clone(),
                    from_name: target.object.name.clone(),
                    to_name: source.object.name.clone(),
                    similarity_percent: similarity,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.similarity_percent
            .cmp(&a.similarity_percent)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.schema.cmp(&b.schema))
            .then_with(|| a.to_name.cmp(&b.to_name))
    });
    let mut used_from = std::collections::HashSet::<(String, String, String)>::new();
    let mut used_to = std::collections::HashSet::<(String, String, String)>::new();
    let mut accepted = Vec::new();
    for candidate in candidates {
        let from_key = (
            candidate.kind.clone(),
            candidate.schema.clone(),
            candidate.from_name.clone(),
        );
        let to_key = (
            candidate.kind.clone(),
            candidate.schema.clone(),
            candidate.to_name.clone(),
        );
        if used_from.contains(&from_key) || used_to.contains(&to_key) {
            continue;
        }
        used_from.insert(from_key);
        used_to.insert(to_key);
        accepted.push(candidate);
    }
    accepted.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.schema.cmp(&b.schema))
            .then_with(|| a.to_name.cmp(&b.to_name))
    });
    accepted
}

pub fn apply_renames(differences: &mut Vec<ComparedDifference>, accepted: &[RenameCandidate]) {
    for rename in accepted {
        let from_key = (
            rename.kind.as_str(),
            rename.schema.as_str(),
            rename.from_name.as_str(),
        );
        let to_key = (
            rename.kind.as_str(),
            rename.schema.as_str(),
            rename.to_name.as_str(),
        );
        let extra_idx = differences.iter().position(|d| {
            d.status == DifferenceStatus::ExtraInTarget
                && object_key(&d.object)
                    == (
                        rename.kind.as_str(),
                        rename.schema.as_str(),
                        rename.from_name.as_str(),
                    )
        });
        let missing_idx = differences.iter().position(|d| {
            d.status == DifferenceStatus::MissingInTarget
                && object_key(&d.object)
                    == (
                        rename.kind.as_str(),
                        rename.schema.as_str(),
                        rename.to_name.as_str(),
                    )
        });
        let (Some(extra_idx), Some(missing_idx)) = (extra_idx, missing_idx) else {
            continue;
        };
        let extra = differences.remove(extra_idx);
        let missing_idx = if missing_idx > extra_idx {
            missing_idx - 1
        } else {
            missing_idx
        };
        let missing = differences.remove(missing_idx);
        differences.push(ComparedDifference {
            status: DifferenceStatus::Renamed {
                from: rename.from_name.clone(),
            },
            object: missing.object,
            target_definition: Some(extra.object.definition),
            note: "Rename accepted by the user.".to_string(),
        });
        let _ = (from_key, to_key);
    }
    differences.sort_by(|left, right| object_key(&left.object).cmp(&object_key(&right.object)));
}

pub fn comment_block(text: &str) -> String {
    text.lines()
        .map(|line| format!("-- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn definition_is_runnable(object: &SchemaObject) -> bool {
    let trimmed = object.definition.trim_start();
    !trimmed.starts_with("--")
        && ["CREATE ", "ALTER "]
            .iter()
            .any(|prefix| trimmed.to_ascii_uppercase().starts_with(prefix))
}

fn bracket_quote(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

fn pg_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn mysql_quote(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// `` `db`.`name` `` for ClickHouse (backticks, like MySQL); a bare name
/// when the object carries no database.
fn ch_qualified(schema: &str, name: &str) -> String {
    let quote = |s: &str| DatabaseType::ClickHouse.quote_identifier(s);
    if schema.is_empty() {
        quote(name)
    } else {
        format!("{}.{}", quote(schema), quote(name))
    }
}

/// Turn ClickHouse's `CREATE VIEW …` / `CREATE DICTIONARY …` (from
/// `system.tables.create_table_query`) into the `CREATE OR REPLACE` form,
/// which ClickHouse accepts for views and dictionaries.
fn ch_create_or_replace(definition: &str) -> String {
    let upper = definition.to_ascii_uppercase();
    for prefix in ["CREATE VIEW", "CREATE DICTIONARY"] {
        if let Some(idx) = upper.find(prefix) {
            let keyword = &prefix["CREATE ".len()..];
            return format!(
                "{}CREATE OR REPLACE {keyword}{}",
                &definition[..idx],
                &definition[idx + prefix.len()..]
            );
        }
    }
    definition.to_string()
}

fn oracle_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `"SCHEMA"."NAME"`, or just `"NAME"` when the schema part is empty.
fn oracle_qualified(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        oracle_quote(name)
    } else {
        format!("{}.{}", oracle_quote(schema), oracle_quote(name))
    }
}

/// The Oracle object type behind a `ROUTINE`/`TYPE` definition — the word
/// after `CREATE [OR REPLACE] [[NON]EDITIONABLE]` in its `DBMS_METADATA`
/// DDL (`PROCEDURE`, `FUNCTION`, `PACKAGE`, `PACKAGE BODY`, `TYPE`,
/// `TYPE BODY`), or `None` when the definition is not such a header.
fn oracle_routine_kind(definition: &str) -> Option<&'static str> {
    let upper = definition.trim_start().to_ascii_uppercase();
    let mut words = upper.split_whitespace().peekable();
    if words.next() != Some("CREATE") {
        return None;
    }
    while matches!(
        words.peek(),
        Some(&"OR" | &"REPLACE" | &"EDITIONABLE" | &"NONEDITIONABLE")
    ) {
        words.next();
    }
    let head = words.next()?;
    let body = words.peek().is_some_and(|w| w.trim_matches('"') == "BODY");
    Some(match (head, body) {
        ("PROCEDURE", _) => "PROCEDURE",
        ("FUNCTION", _) => "FUNCTION",
        ("PACKAGE", true) => "PACKAGE BODY",
        ("PACKAGE", false) => "PACKAGE",
        ("TYPE", true) => "TYPE BODY",
        ("TYPE", false) => "TYPE",
        _ => return None,
    })
}

/// A constraint key whose name was system-generated is rendered as
/// `table.(PRIMARY KEY "ID")` by the collector; such a constraint cannot be
/// addressed by name in `DROP CONSTRAINT`.
fn oracle_constraint_name(name: &str) -> Option<(&str, &str)> {
    let (table, constraint) = name.split_once('.')?;
    (!constraint.starts_with('(')).then_some((table, constraint))
}

/// Oracle `DROP` statement for an object the shared `drop_sql` arms do not
/// cover, or a manual-review comment when the object cannot be addressed by
/// name (generated constraint names, routines whose DDL header is
/// unreadable).
fn oracle_drop_sql(object: &SchemaObject) -> String {
    let schema = &object.schema;
    let name = &object.name;
    let manual = || format!("-- MANUAL REVIEW: DROP {} {schema}.{name}", object.kind);
    match object.kind.as_str() {
        "SEQUENCE" | "SYNONYM" | "MATERIALIZED VIEW" => {
            format!("DROP {} {};", object.kind, oracle_qualified(schema, name))
        }
        "ROUTINE" | "TYPE" => {
            let Some(kind) = oracle_routine_kind(&object.definition) else {
                return manual();
            };
            let bare = name.trim_end_matches(" (BODY)");
            format!("DROP {kind} {};", oracle_qualified(schema, bare))
        }
        "TRIGGER" => {
            let trigger = name.rsplit_once('.').map(|(_, t)| t).unwrap_or(name);
            format!("DROP TRIGGER {};", oracle_qualified(schema, trigger))
        }
        "PRIMARY KEY" | "UNIQUE CONSTRAINT" | "CHECK CONSTRAINT" | "FOREIGN KEY" => {
            match oracle_constraint_name(name) {
                Some((table, constraint)) => format!(
                    "ALTER TABLE {} DROP CONSTRAINT {};",
                    oracle_qualified(schema, table),
                    oracle_quote(constraint)
                ),
                None => manual(),
            }
        }
        _ => manual(),
    }
}

fn qualified_identity(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// Deploy order by kind. The Oracle-only kinds slot around the shared ones:
/// sequences and types before the tables whose defaults and columns use
/// them, materialized views and synonyms after the views they may select
/// from.
fn kind_rank(kind: &str) -> u8 {
    match kind {
        // A column default may draw from a sequence, so sequences precede
        // the tables that use them.
        "SEQUENCE" => 0,
        "TYPE" => 1,
        "TABLE" => 2,
        "PRIMARY KEY" => 3,
        "UNIQUE CONSTRAINT" => 4,
        "DEFAULT CONSTRAINT" => 5,
        "CHECK CONSTRAINT" => 6,
        "INDEX" => 7,
        "FOREIGN KEY" => 8,
        "VIEW" => 9,
        "MATERIALIZED VIEW" => 10,
        "SYNONYM" => 11,
        "ROUTINE" => 12,
        "TRIGGER" => 13,
        _ => 14,
    }
}

/// Turn a stored `CREATE VIEW|PROCEDURE|PROC|FUNCTION|TRIGGER …` definition
/// into its replace-in-place form by inserting `or_words` (`OR ALTER` on SQL
/// Server, `OR REPLACE` on PostgreSQL) right after the statement's leading
/// `CREATE`. Everything else is kept byte for byte: the comments above the
/// statement (SQL Server stores them as part of the object), the spacing,
/// and the object keyword. Only the statement's first token counts, so a
/// `CREATE VIEW` inside a comment, a string, or a procedure body is never
/// rewritten. A definition already in the `CREATE OR …` form, or one that
/// does not start with such a CREATE, is returned unchanged.
fn rewrite_create_prefix(definition: &str, dialect: Dialect, or_words: &str) -> String {
    let mut words = StrictLexer::tokenize(definition, dialect)
        .into_iter()
        .filter(|token| !token.is_trivia());
    let Some(create) = words.next() else {
        return definition.to_string();
    };
    if !(create.is_word() && create.text.eq_ignore_ascii_case("CREATE")) {
        return definition.to_string();
    }
    let Some(object) = words.next() else {
        return definition.to_string();
    };
    let replaceable = object.is_word()
        && ["VIEW", "PROCEDURE", "PROC", "FUNCTION", "TRIGGER"]
            .iter()
            .any(|keyword| object.text.eq_ignore_ascii_case(keyword));
    if !replaceable {
        return definition.to_string();
    }
    // SQL Server stores `CREATE OR ALTER VIEW` as `CREATE   VIEW` (the
    // OR ALTER blanked out), so keeping that gap would add two spaces on
    // every deploy. A whitespace-only gap becomes one space; a gap holding
    // a comment is kept.
    let gap = &definition[create.span.end..object.span.start];
    if gap.trim().is_empty() {
        format!(
            "{} {or_words} {}",
            &definition[..create.span.end],
            &definition[object.span.start..]
        )
    } else {
        format!(
            "{} {or_words}{}",
            &definition[..create.span.end],
            &definition[create.span.end..]
        )
    }
}

fn changed_definition_sql(
    difference: &ComparedDifference,
    target_type: DatabaseType,
    target_postgres_major: Option<u32>,
    plan: &mut DeployPlan,
) -> (String, String, bool) {
    let identity = qualified_identity(&difference.object.schema, &difference.object.name);
    let target_def = difference.target_definition.as_deref().unwrap_or_default();
    match difference.object.kind.as_str() {
        "TABLE" => (
            format!(
                "-- MANUAL REVIEW: table columns differ for {identity}\n-- Source signature:\n{}\n-- Target signature:\n{}",
                comment_block(&difference.object.definition),
                comment_block(target_def)
            ),
            "-- MANUAL REVIEW".to_string(),
            false,
        ),
        "VIEW" | "ROUTINE" | "TRIGGER" => match target_type.connection_kind() {
            ConnectionKind::Tds => {
                let sql = rewrite_create_prefix(
                    &difference.object.definition,
                    Dialect::SqlServer,
                    "OR ALTER",
                );
                let rollback = rewrite_create_prefix(target_def, Dialect::SqlServer, "OR ALTER");
                (sql, rollback, true)
            }
            ConnectionKind::PostgresWire => {
                if difference.object.kind == "TRIGGER"
                    && !target_postgres_major.is_some_and(|major| major >= 14)
                {
                    // `CREATE OR REPLACE TRIGGER` arrived in PostgreSQL 14: warn
                    // firmly on a known older server, softly when the version
                    // is not known.
                    let warning = if target_postgres_major.is_some() {
                        sqlly_core::DisplayMessage::new(
                            "services.schema_compare.warnings.postgres_trigger",
                            format!(
                                "PostgreSQL before version 14 has no CREATE OR REPLACE TRIGGER for {identity}; review manually."
                            ),
                        )
                        .with_arg("object_name", identity.clone())
                    } else {
                        sqlly_core::DisplayMessage::new(
                            "services.schema_compare.warnings.postgres_trigger_unknown_version",
                            format!(
                                "The target PostgreSQL version is not known, and CREATE OR REPLACE TRIGGER needs version 14 or later, so {identity} is left for manual review."
                            ),
                        )
                        .with_arg("object_name", identity.clone())
                    };
                    plan.warnings.push(warning);
                    (
                        format!(
                            "-- MANUAL REVIEW: trigger change for {identity}\n{}",
                            comment_block(&difference.object.definition)
                        ),
                        comment_block(target_def),
                        false,
                    )
                } else {
                    let sql = rewrite_create_prefix(
                        &difference.object.definition,
                        Dialect::Postgres,
                        "OR REPLACE",
                    );
                    let rollback =
                        rewrite_create_prefix(target_def, Dialect::Postgres, "OR REPLACE");
                    (sql, rollback, true)
                }
            }
            ConnectionKind::MySqlWire => {
                let drop = format!(
                    "DROP {} IF EXISTS {};",
                    difference.object.kind,
                    mysql_quote(&qualified_identity(
                        &difference.object.schema,
                        &difference.object.name
                    ))
                );
                (
                    format!("{drop}\n{}", difference.object.definition),
                    format!("{drop}\n{target_def}"),
                    true,
                )
            }
            ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
                let drop = format!("DROP VIEW IF EXISTS {};", difference.object.name);
                (
                    format!("{drop}\n{}", difference.object.definition),
                    format!("{drop}\n{target_def}"),
                    true,
                )
            }
            // DuckDB replaces views and macros in place; it has no triggers.
            ConnectionKind::DuckDbFile if difference.object.kind != "TRIGGER" => {
                let sql = duckdb_create_or_replace(&difference.object.definition);
                let rollback = duckdb_create_or_replace(target_def);
                (sql, rollback, true)
            }
            // ClickHouse has CREATE OR REPLACE VIEW (and no routines or
            // triggers to reach this arm).
            ConnectionKind::ClickHouseHttp if difference.object.kind == "VIEW" => (
                ch_create_or_replace(&difference.object.definition),
                ch_create_or_replace(target_def),
                true,
            ),
            // DBMS_METADATA renders views, routines and triggers as
            // `CREATE OR REPLACE …`, which replaces in place.
            ConnectionKind::OracleNet => (
                difference.object.definition.clone(),
                target_def.to_string(),
                true,
            ),
            _ => (
                format!(
                    "-- MANUAL REVIEW: {identity}\n{}",
                    comment_block(&difference.object.definition)
                ),
                comment_block(target_def),
                false,
            ),
        },
        "DICTIONARY" if target_type.connection_kind() == ConnectionKind::ClickHouseHttp => (
            ch_create_or_replace(&difference.object.definition),
            ch_create_or_replace(target_def),
            true,
        ),
        // A materialized view has no CREATE OR REPLACE; dropping it discards
        // the rows in its implicit inner table, so the plan says so.
        "MATERIALIZED VIEW" if target_type.connection_kind() == ConnectionKind::ClickHouseHttp => {
            plan.warnings.push(
                sqlly_core::DisplayMessage::new(
                    "services.schema_compare.warnings.materialized_view_rows",
                    format!("Recreating materialized view {identity} drops the rows stored in its inner table unless it targets an explicit TO table."),
                )
                .with_arg("object_name", identity.clone()),
            );
            let drop = format!(
                "DROP VIEW IF EXISTS {};",
                ch_qualified(&difference.object.schema, &difference.object.name)
            );
            (
                format!("{drop}\n{}", difference.object.definition),
                format!("{drop}\n{target_def}"),
                true,
            )
        }
        // Oracle synonyms and types also come out as `CREATE OR REPLACE`; a
        // materialized view has no replace form, so it is dropped and
        // recreated from its captured DDL.
        "SYNONYM" | "TYPE" if target_type.connection_kind() == ConnectionKind::OracleNet => (
            difference.object.definition.clone(),
            target_def.to_string(),
            true,
        ),
        "MATERIALIZED VIEW" if target_type.connection_kind() == ConnectionKind::OracleNet => {
            let drop = oracle_drop_sql(&difference.object);
            (
                format!("{drop}\n{}", difference.object.definition),
                format!("{drop}\n{target_def}"),
                true,
            )
        }
        _ => (
            format!(
                "-- MANUAL REVIEW: {identity}\n{}",
                comment_block(&difference.object.definition)
            ),
            comment_block(target_def),
            false,
        ),
    }
}

/// `CREATE VIEW` / `CREATE MACRO` → the `CREATE OR REPLACE` form DuckDB
/// accepts for both; anything else is returned unchanged.
fn duckdb_create_or_replace(definition: &str) -> String {
    let upper = definition.to_ascii_uppercase();
    for prefix in ["CREATE VIEW", "CREATE MACRO"] {
        if let Some(idx) = upper.find(prefix) {
            let keyword = &prefix["CREATE ".len()..];
            return format!(
                "{}CREATE OR REPLACE {keyword}{}",
                &definition[..idx],
                &definition[idx + prefix.len()..]
            );
        }
    }
    definition.to_string()
}

fn rename_sql(
    rename_from: &str,
    object: &SchemaObject,
    target_type: DatabaseType,
) -> (String, String) {
    let schema = &object.schema;
    let to_name = &object.name;
    match target_type.connection_kind() {
        ConnectionKind::Tds => {
            let from = if schema.is_empty() {
                bracket_quote(rename_from)
            } else {
                format!("{}.{}", bracket_quote(schema), bracket_quote(rename_from))
            };
            let to = if schema.is_empty() {
                bracket_quote(to_name)
            } else {
                format!("{}.{}", bracket_quote(schema), bracket_quote(to_name))
            };
            (
                format!("EXEC sp_rename N'{from}', N'{to}';"),
                format!("EXEC sp_rename N'{to}', N'{from}';"),
            )
        }
        ConnectionKind::PostgresWire => match object.kind.as_str() {
            "TABLE" => (
                format!(
                    "ALTER TABLE {} RENAME TO {};",
                    pg_quote(&format!("{schema}.{rename_from}")),
                    pg_quote(to_name)
                ),
                format!(
                    "ALTER TABLE {} RENAME TO {};",
                    pg_quote(&format!("{schema}.{to_name}")),
                    pg_quote(rename_from)
                ),
            ),
            "VIEW" => (
                format!(
                    "ALTER VIEW {} RENAME TO {};",
                    pg_quote(&format!("{schema}.{rename_from}")),
                    pg_quote(to_name)
                ),
                format!(
                    "ALTER VIEW {} RENAME TO {};",
                    pg_quote(&format!("{schema}.{to_name}")),
                    pg_quote(rename_from)
                ),
            ),
            _ => (
                format!(
                    "-- Manual review: routine rename {}.{} -> {}",
                    schema, rename_from, to_name
                ),
                format!(
                    "-- Manual review: routine rename {}.{} -> {}",
                    schema, to_name, rename_from
                ),
            ),
        },
        ConnectionKind::MySqlWire => {
            if object.kind == "TABLE" {
                (
                    format!(
                        "RENAME TABLE {} TO {};",
                        mysql_quote(&format!("{schema}.{rename_from}")),
                        mysql_quote(&format!("{schema}.{to_name}"))
                    ),
                    format!(
                        "RENAME TABLE {} TO {};",
                        mysql_quote(&format!("{schema}.{to_name}")),
                        mysql_quote(&format!("{schema}.{rename_from}"))
                    ),
                )
            } else {
                (
                    format!(
                        "-- Manual review: rename {}.{} -> {}",
                        schema, rename_from, to_name
                    ),
                    format!(
                        "-- Manual review: rename {}.{} -> {}",
                        schema, to_name, rename_from
                    ),
                )
            }
        }
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            if object.kind == "TABLE" {
                (
                    format!("ALTER TABLE {rename_from} RENAME TO {to_name};"),
                    format!("ALTER TABLE {to_name} RENAME TO {rename_from};"),
                )
            } else {
                (
                    format!("-- Manual review: rename {rename_from} -> {to_name}"),
                    format!("-- Manual review: rename {to_name} -> {rename_from}"),
                )
            }
        }
        ConnectionKind::DuckDbFile => match object.kind.as_str() {
            "TABLE" | "VIEW" => {
                let kind = &object.kind;
                let qualify = |name: &str| {
                    if schema.is_empty() {
                        pg_quote(name)
                    } else {
                        format!("{}.{}", pg_quote(schema), pg_quote(name))
                    }
                };
                (
                    format!(
                        "ALTER {kind} {} RENAME TO {};",
                        qualify(rename_from),
                        pg_quote(to_name)
                    ),
                    format!(
                        "ALTER {kind} {} RENAME TO {};",
                        qualify(to_name),
                        pg_quote(rename_from)
                    ),
                )
            }
            _ => (
                format!("-- Manual review: rename {schema}.{rename_from} -> {to_name}"),
                format!("-- Manual review: rename {schema}.{to_name} -> {rename_from}"),
            ),
        },
        // `RENAME old TO new` covers views and sequences but only in the
        // session's own schema, so it carries no owner prefix.
        ConnectionKind::OracleNet => match object.kind.as_str() {
            "TABLE" => (
                format!(
                    "ALTER TABLE {} RENAME TO {};",
                    oracle_qualified(schema, rename_from),
                    oracle_quote(to_name)
                ),
                format!(
                    "ALTER TABLE {} RENAME TO {};",
                    oracle_qualified(schema, to_name),
                    oracle_quote(rename_from)
                ),
            ),
            "VIEW" | "SEQUENCE" => (
                format!(
                    "RENAME {} TO {};",
                    oracle_quote(rename_from),
                    oracle_quote(to_name)
                ),
                format!(
                    "RENAME {} TO {};",
                    oracle_quote(to_name),
                    oracle_quote(rename_from)
                ),
            ),
            _ => (
                format!("-- Manual review: rename {schema}.{rename_from} -> {to_name}"),
                format!("-- Manual review: rename {schema}.{to_name} -> {rename_from}"),
            ),
        },
        // RENAME TABLE covers tables, views and materialized views;
        // dictionaries have their own verb.
        ConnectionKind::ClickHouseHttp => {
            let verb = match object.kind.as_str() {
                "TABLE" | "VIEW" | "MATERIALIZED VIEW" => Some("RENAME TABLE"),
                "DICTIONARY" => Some("RENAME DICTIONARY"),
                _ => None,
            };
            match verb {
                Some(verb) => (
                    format!(
                        "{verb} {} TO {};",
                        ch_qualified(schema, rename_from),
                        ch_qualified(schema, to_name)
                    ),
                    format!(
                        "{verb} {} TO {};",
                        ch_qualified(schema, to_name),
                        ch_qualified(schema, rename_from)
                    ),
                ),
                None => (
                    format!("-- Manual review: rename {schema}.{rename_from} -> {to_name}"),
                    format!("-- Manual review: rename {schema}.{to_name} -> {rename_from}"),
                ),
            }
        }
        _ => (
            format!("-- Manual review: rename {schema}.{rename_from} -> {to_name}"),
            format!("-- Manual review: rename {schema}.{to_name} -> {rename_from}"),
        ),
    }
}

fn drop_sql(object: &SchemaObject, target_type: DatabaseType) -> String {
    let schema = &object.schema;
    let name = &object.name;
    match object.kind.as_str() {
        "TABLE" => match target_type.connection_kind() {
            ConnectionKind::Tds => format!(
                "DROP TABLE {}.{};",
                bracket_quote(schema),
                bracket_quote(name)
            ),
            ConnectionKind::PostgresWire => {
                format!("DROP TABLE {}.{};", pg_quote(schema), pg_quote(name))
            }
            ConnectionKind::MySqlWire => {
                format!("DROP TABLE {}.{};", mysql_quote(schema), mysql_quote(name))
            }
            ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
                format!("DROP TABLE {name};")
            }
            ConnectionKind::DuckDbFile => {
                format!("DROP TABLE {}.{};", pg_quote(schema), pg_quote(name))
            }
            ConnectionKind::ClickHouseHttp => {
                format!("DROP TABLE {};", ch_qualified(schema, name))
            }
            ConnectionKind::OracleNet => {
                format!("DROP TABLE {};", oracle_qualified(schema, name))
            }
            _ => format!("-- MANUAL REVIEW: DROP TABLE {schema}.{name}"),
        },
        "FOREIGN KEY" if target_type.connection_kind() == ConnectionKind::OracleNet => {
            oracle_drop_sql(object)
        }
        "FOREIGN KEY" => {
            let (table, fk) = name.split_once('.').unwrap_or((name, name));
            match target_type.connection_kind() {
                ConnectionKind::Tds => format!(
                    "ALTER TABLE {}.{} DROP CONSTRAINT {};",
                    bracket_quote(schema),
                    bracket_quote(table),
                    bracket_quote(fk)
                ),
                ConnectionKind::PostgresWire => format!(
                    "ALTER TABLE {}.{} DROP CONSTRAINT {};",
                    pg_quote(schema),
                    pg_quote(table),
                    pg_quote(fk)
                ),
                ConnectionKind::MySqlWire => format!(
                    "ALTER TABLE {}.{} DROP FOREIGN KEY {};",
                    mysql_quote(schema),
                    mysql_quote(table),
                    mysql_quote(fk)
                ),
                // DuckDB cannot drop a constraint in place; the table has to
                // be recreated without it.
                ConnectionKind::DuckDbFile => format!(
                    "-- MANUAL REVIEW: DuckDB has no DROP CONSTRAINT; recreate {schema}.{table} without foreign key {fk}"
                ),
                _ => format!("-- MANUAL REVIEW: DROP FOREIGN KEY {schema}.{name}"),
            }
        }
        "INDEX" => {
            let (table, index) = name.split_once('.').unwrap_or((name, name));
            match target_type.connection_kind() {
                ConnectionKind::Tds => format!(
                    "DROP INDEX {} ON {}.{};",
                    bracket_quote(index),
                    bracket_quote(schema),
                    bracket_quote(table)
                ),
                ConnectionKind::PostgresWire => {
                    format!("DROP INDEX {}.{};", pg_quote(schema), pg_quote(index))
                }
                ConnectionKind::MySqlWire => format!(
                    "DROP INDEX {} ON {}.{};",
                    mysql_quote(index),
                    mysql_quote(schema),
                    mysql_quote(table)
                ),
                ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
                    format!("DROP INDEX {name};")
                }
                ConnectionKind::DuckDbFile => {
                    format!("DROP INDEX {}.{};", pg_quote(schema), pg_quote(index))
                }
                ConnectionKind::ClickHouseHttp => format!(
                    "ALTER TABLE {} DROP INDEX {};",
                    ch_qualified(schema, table),
                    mysql_quote(index)
                ),
                ConnectionKind::OracleNet => {
                    format!("DROP INDEX {};", oracle_qualified(schema, index))
                }
                _ => format!("-- MANUAL REVIEW: DROP INDEX {schema}.{name}"),
            }
        }
        "VIEW" => match target_type.connection_kind() {
            ConnectionKind::Tds => format!(
                "DROP VIEW {}.{};",
                bracket_quote(schema),
                bracket_quote(name)
            ),
            ConnectionKind::PostgresWire => {
                format!("DROP VIEW {}.{};", pg_quote(schema), pg_quote(name))
            }
            ConnectionKind::MySqlWire => {
                format!("DROP VIEW {}.{};", mysql_quote(schema), mysql_quote(name))
            }
            ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
                format!("DROP VIEW IF EXISTS {name};")
            }
            ConnectionKind::DuckDbFile => {
                format!("DROP VIEW {}.{};", pg_quote(schema), pg_quote(name))
            }
            ConnectionKind::ClickHouseHttp => {
                format!("DROP VIEW {};", ch_qualified(schema, name))
            }
            ConnectionKind::OracleNet => {
                format!("DROP VIEW {};", oracle_qualified(schema, name))
            }
            _ => format!("-- MANUAL REVIEW: DROP VIEW {schema}.{name}"),
        },
        _ if target_type.connection_kind() == ConnectionKind::OracleNet => oracle_drop_sql(object),
        "SEQUENCE" if target_type.connection_kind() == ConnectionKind::DuckDbFile => {
            format!("DROP SEQUENCE {}.{};", pg_quote(schema), pg_quote(name))
        }
        "ROUTINE" if target_type.connection_kind() == ConnectionKind::DuckDbFile => {
            format!("DROP MACRO {}.{};", pg_quote(schema), pg_quote(name))
        }
        "MATERIALIZED VIEW" | "DICTIONARY" => match target_type.connection_kind() {
            ConnectionKind::ClickHouseHttp => format!(
                "DROP {} {};",
                if object.kind == "DICTIONARY" {
                    "DICTIONARY"
                } else {
                    "VIEW"
                },
                ch_qualified(schema, name)
            ),
            _ => format!("-- MANUAL REVIEW: DROP {} {schema}.{name}", object.kind),
        },
        _ => format!("-- MANUAL REVIEW: DROP {} {}.{}", object.kind, schema, name),
    }
}

fn rollback_for_create(object: &SchemaObject, target_type: DatabaseType) -> String {
    if object.kind == "TABLE" {
        format!(
            "{} -- data loss if executed",
            drop_sql(object, target_type).trim_end_matches(';')
        )
    } else {
        format!(
            "-- MANUAL REVIEW: rollback for created {}",
            qualified_identity(&object.schema, &object.name)
        )
    }
}

fn step_for_difference(
    difference: &ComparedDifference,
    source_type: DatabaseType,
    target_type: DatabaseType,
    include_drops: bool,
    target_postgres_major: Option<u32>,
    plan: &mut DeployPlan,
) -> Option<DeployStep> {
    let same_engine = source_type.connection_kind() == target_type.connection_kind();
    let identity = qualified_identity(&difference.object.schema, &difference.object.name);
    let (sql, rollback_sql, runnable) = match &difference.status {
        DifferenceStatus::MissingInTarget
            if same_engine && definition_is_runnable(&difference.object) =>
        {
            (
                difference.object.definition.clone(),
                rollback_for_create(&difference.object, target_type),
                true,
            )
        }
        DifferenceStatus::MissingInTarget => (
            format!(
                "-- Missing in target: {identity}\n-- Manual target-engine translation may be lossy; source definition follows as comments.\n{}",
                comment_block(&difference.object.definition)
            ),
            "-- MANUAL REVIEW".to_string(),
            false,
        ),
        DifferenceStatus::ExtraInTarget if include_drops && same_engine => {
            let sql = drop_sql(&difference.object, target_type);
            let rollback = if definition_is_runnable(&difference.object) {
                difference.object.definition.clone()
            } else {
                format!(
                    "-- MANUAL REVIEW: recreate {identity} from a backup; definition not captured"
                )
            };
            (sql, rollback, true)
        }
        DifferenceStatus::ExtraInTarget => (
            format!(
                "-- Extra in target: {identity}\n-- No DROP statement is generated; review manually."
            ),
            "-- MANUAL REVIEW".to_string(),
            false,
        ),
        DifferenceStatus::Changed | DifferenceStatus::ManualReview if same_engine => {
            changed_definition_sql(difference, target_type, target_postgres_major, plan)
        }
        DifferenceStatus::Changed | DifferenceStatus::ManualReview => (
            format!(
                "-- Manual review: {identity}\n-- Source definition:\n{}\n-- Target definition:\n{}",
                comment_block(&difference.object.definition),
                comment_block(difference.target_definition.as_deref().unwrap_or_default())
            ),
            "-- MANUAL REVIEW".to_string(),
            false,
        ),
        DifferenceStatus::Renamed { from } if same_engine => {
            let (rename_forward, rename_back) = rename_sql(from, &difference.object, target_type);
            if let Some(target_def) = difference.target_definition.as_deref()
                && comparable_definition(target_def)
                    != comparable_definition(&difference.object.definition)
            {
                let changed = ComparedDifference {
                    status: DifferenceStatus::Changed,
                    object: difference.object.clone(),
                    target_definition: Some(target_def.to_string()),
                    note: difference.note.clone(),
                };
                let (changed_sql, changed_rollback, _) =
                    changed_definition_sql(&changed, target_type, target_postgres_major, plan);
                return Some(DeployStep {
                    identity: identity.clone(),
                    kind: difference.object.kind.clone(),
                    status: difference.status.clone(),
                    definition: difference.object.definition.clone(),
                    sql: format!("{rename_forward}\n{changed_sql}"),
                    rollback_sql: format!("{changed_rollback}\n{rename_back}"),
                    runnable: true,
                });
            }
            (rename_forward, rename_back, true)
        }
        DifferenceStatus::Renamed { .. } => (
            format!("-- Manual review: rename {identity}"),
            "-- MANUAL REVIEW".to_string(),
            false,
        ),
        DifferenceStatus::Unsupported => (
            format!(
                "-- Unsupported comparison: {identity}\n-- {}",
                difference.note
            ),
            "-- MANUAL REVIEW".to_string(),
            false,
        ),
    };
    Some(DeployStep {
        identity,
        kind: difference.object.kind.clone(),
        status: difference.status.clone(),
        definition: difference.object.definition.clone(),
        sql,
        rollback_sql,
        runnable,
    })
}

fn view_references(definition: &str, other_name: &str) -> bool {
    let upper = definition.to_ascii_uppercase();
    let name_upper = other_name.to_ascii_uppercase();
    [
        format!("[{name_upper}]"),
        format!("\"{name_upper}\""),
        format!("`{name_upper}`"),
        format!(" {name_upper} "),
        format!(" {name_upper}."),
        format!(".{name_upper} "),
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

fn order_view_steps(steps: &mut [DeployStep], warnings: &mut Vec<sqlly_core::DisplayMessage>) {
    let len = steps.len();
    for _ in 0..len.saturating_mul(len) {
        let mut swapped = false;
        for i in 0..len.saturating_sub(1) {
            let (left, right) = steps.split_at_mut(i + 1);
            let a = &left[i];
            let b = &mut right[0];
            let a_view = a.kind == "VIEW" || a.kind == "ROUTINE";
            let b_view = b.kind == "VIEW" || b.kind == "ROUTINE";
            if a_view
                && b_view
                && view_references(&a.definition, &b.object_name())
                && view_references(&b.definition, &a.object_name())
            {
                warnings.push(
                    sqlly_core::DisplayMessage::new(
                        "services.schema_compare.warnings.view_order",
                        format!(
                            "Could not order views {} and {}; review manually.",
                            a.identity, b.identity
                        ),
                    )
                    .with_arg("first_object", a.identity.clone())
                    .with_arg("second_object", b.identity.clone()),
                );
            } else if a_view
                && b_view
                && view_references(&a.definition, &b.object_name())
                && !view_references(&b.definition, &a.object_name())
            {
                std::mem::swap(&mut left[i], &mut right[0]);
                swapped = true;
            }
        }
        if !swapped {
            break;
        }
    }
}

impl DeployStep {
    fn object_name(&self) -> String {
        self.identity
            .rsplit_once('.')
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| self.identity.clone())
    }
}

pub fn plan_deploy(
    differences: &[ComparedDifference],
    source_type: DatabaseType,
    target_type: DatabaseType,
    options: DeployOptions,
) -> DeployPlan {
    let mut plan = DeployPlan::default();
    let mut renames = Vec::new();
    let mut ups = Vec::new();
    let mut drops = Vec::new();
    for difference in differences {
        let is_drop = difference.status == DifferenceStatus::ExtraInTarget && options.include_drops;
        let is_rename = matches!(difference.status, DifferenceStatus::Renamed { .. });
        let step = step_for_difference(
            difference,
            source_type,
            target_type,
            options.include_drops,
            options.target_postgres_major,
            &mut plan,
        );
        let Some(step) = step else { continue };
        if is_rename {
            renames.push(step);
        } else if is_drop {
            drops.push(step);
        } else if difference.status != DifferenceStatus::ExtraInTarget || !options.include_drops {
            ups.push(step);
        }
    }
    renames.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.identity.cmp(&b.identity))
    });
    ups.sort_by(|a, b| {
        kind_rank(&a.kind)
            .cmp(&kind_rank(&b.kind))
            .then_with(|| a.identity.cmp(&b.identity))
    });
    order_view_steps(&mut ups, &mut plan.warnings);
    drops.sort_by(|a, b| {
        kind_rank(&b.kind)
            .cmp(&kind_rank(&a.kind))
            .then_with(|| a.identity.cmp(&b.identity))
    });
    plan.steps.extend(renames);
    plan.steps.extend(ups);
    plan.steps.extend(drops);
    let _ = options.wrap_in_transaction;
    plan
}

pub fn deploy_script(plan: &DeployPlan, header: &str) -> String {
    let mut parts = Vec::new();
    if !header.is_empty() {
        parts.push(header.trim_end().to_string());
    }
    for step in &plan.steps {
        parts.push(step.sql.trim().to_string());
    }
    parts.join(";\n")
}

pub fn rollback_script(plan: &DeployPlan, header: &str) -> String {
    let mut parts = Vec::new();
    if !header.is_empty() {
        parts.push(header.trim_end().to_string());
    }
    for step in plan.steps.iter().rev() {
        if step.runnable || !step.rollback_sql.starts_with("-- MANUAL REVIEW") {
            parts.push(step.rollback_sql.trim().to_string());
        }
    }
    parts.join(";\n")
}

pub fn script_differences(
    differences: &[ComparedDifference],
    source_type: DatabaseType,
    target_type: DatabaseType,
) -> String {
    let plan = plan_deploy(
        differences,
        source_type,
        target_type,
        DeployOptions::default(),
    );
    deploy_script(&plan, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickhouse_qualified_name_escapes_backslashes() {
        assert_eq!(ch_qualified(r"db\", "t"), r"`db\\`.`t`");
    }
    use crate::admin::advanced_tools::SchemaObject;

    fn obj(schema: &str, name: &str, kind: &str, def: &str) -> SchemaObject {
        SchemaObject {
            schema: schema.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            definition: def.to_string(),
        }
    }

    /// The Oracle collector is one session-scoped batch: the DBMS_METADATA
    /// transform block first, then one four-column SELECT per kind, every
    /// SELECT scoped to the current schema and read-only.
    #[test]
    fn oracle_collector_batch_has_the_transform_block_and_one_select_per_kind() {
        let sql = oracle_collect_objects_sql();
        let statements: Vec<&str> = sql.split(";\n").collect();
        assert!(statements[0].starts_with(
            "BEGIN IF NVL(SYS_CONTEXT('USERENV', 'CLIENT_INFO'), ' ') <> 'SQLLY_DDL_V1' THEN DBMS_METADATA.SET_TRANSFORM_PARAM"
        ));
        assert!(
            statements[0]
                .contains("DBMS_APPLICATION_INFO.SET_CLIENT_INFO('SQLLY_DDL_V1'); END IF;")
        );
        assert!(statements[0].trim_end().ends_with("END;"));
        for param in [
            "'STORAGE', FALSE",
            "'SEGMENT_ATTRIBUTES', FALSE",
            "'TABLESPACE', FALSE",
            "'SQLTERMINATOR', FALSE",
            "'PRETTY', TRUE",
            "'EMIT_SCHEMA', FALSE",
            "'CONSTRAINTS', TRUE",
            "'REF_CONSTRAINTS', FALSE",
        ] {
            assert!(statements[0].contains(param), "{param}");
        }
        let selects = &statements[1..];
        assert_eq!(selects.len(), 10);
        for select in selects {
            assert!(select.starts_with("SELECT "), "{select}");
            assert!(
                select.contains("SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')"),
                "{select}"
            );
            assert!(select.contains(" AS schema_name"), "{select}");
            assert!(select.contains(" AS object_name"), "{select}");
            assert!(select.contains(" AS kind"), "{select}");
            assert!(select.contains(" AS definition"), "{select}");
            // One statement each: no embedded terminator could smuggle a
            // second statement into the read.
            assert!(!select.contains(';'), "{select}");
        }
        for kind in ORACLE_OBJECT_KINDS {
            assert!(sql.contains(&format!("'{kind}'")), "{kind}");
            assert!(supports_kind(DatabaseType::Oracle, kind), "{kind}");
        }
        assert!(!supports_kind(DatabaseType::Oracle, "DEFAULT CONSTRAINT"));
        // Foreign keys are separate objects, so index rows skip the ones
        // that back PK/UNIQUE constraints and table DDL omits REF constraints.
        assert!(sql.contains("k.constraint_type IN ('P', 'U')"));
        assert!(sql.contains("NOT LIKE 'BIN$%'"));
        assert!(sql.contains("'START WITH [0-9]+', 'START WITH 1'"));
        assert!(sql.contains("NOT LIKE 'ISEQ$$%'"));
        assert!(sql.contains("' (BODY)'"));
        assert_eq!(sql.matches("/*+ NO_MERGE */").count(), 8);
    }

    #[test]
    fn oracle_changed_definitions_replace_in_place_and_mvs_are_recreated() {
        let db = DatabaseType::Oracle;
        let view = ComparedDifference {
            status: DifferenceStatus::Changed,
            object: obj(
                "SQLLY",
                "V",
                "VIEW",
                "CREATE OR REPLACE FORCE EDITIONABLE VIEW \"V\" (\"ID\") AS SELECT 1 FROM DUAL",
            ),
            target_definition: Some(
                "CREATE OR REPLACE FORCE EDITIONABLE VIEW \"V\" (\"ID\") AS SELECT 2 FROM DUAL"
                    .into(),
            ),
            note: String::new(),
        };
        let plan = plan_deploy(
            std::slice::from_ref(&view),
            db,
            db,
            DeployOptions::default(),
        );
        assert!(plan.steps[0].runnable);
        assert!(
            plan.steps[0]
                .sql
                .starts_with("CREATE OR REPLACE FORCE EDITIONABLE VIEW")
        );
        assert!(plan.steps[0].rollback_sql.contains("SELECT 2 FROM DUAL"));

        let routine = ComparedDifference {
            object: obj(
                "SQLLY",
                "PKG (BODY)",
                "ROUTINE",
                "CREATE OR REPLACE EDITIONABLE PACKAGE BODY \"PKG\" AS END;",
            ),
            ..view.clone()
        };
        let plan = plan_deploy(&[routine], db, db, DeployOptions::default());
        assert!(plan.steps[0].runnable);
        assert!(
            plan.steps[0]
                .sql
                .starts_with("CREATE OR REPLACE EDITIONABLE PACKAGE BODY")
        );

        let synonym = ComparedDifference {
            object: obj(
                "SQLLY",
                "S",
                "SYNONYM",
                "CREATE OR REPLACE EDITIONABLE SYNONYM \"S\" FOR \"T\"",
            ),
            ..view.clone()
        };
        let plan = plan_deploy(&[synonym], db, db, DeployOptions::default());
        assert!(plan.steps[0].runnable);
        assert!(
            plan.steps[0]
                .sql
                .starts_with("CREATE OR REPLACE EDITIONABLE SYNONYM")
        );

        let mv = ComparedDifference {
            object: obj(
                "SQLLY",
                "MV",
                "MATERIALIZED VIEW",
                "CREATE MATERIALIZED VIEW \"MV\" AS SELECT 1 FROM DUAL",
            ),
            target_definition: Some("CREATE MATERIALIZED VIEW \"MV\" AS SELECT 2 FROM DUAL".into()),
            ..view.clone()
        };
        let plan = plan_deploy(&[mv], db, db, DeployOptions::default());
        assert!(plan.steps[0].runnable);
        assert!(plan.steps[0].sql.starts_with(
            "DROP MATERIALIZED VIEW \"SQLLY\".\"MV\";\nCREATE MATERIALIZED VIEW \"MV\" AS SELECT 1"
        ));
        assert!(plan.steps[0].rollback_sql.contains("SELECT 2 FROM DUAL"));

        // Tables and sequences stay manual review with both definitions.
        let table = ComparedDifference {
            object: obj("SQLLY", "T", "TABLE", "CREATE TABLE \"T\" (\"ID\" NUMBER)"),
            target_definition: Some("CREATE TABLE \"T\" (\"ID\" NUMBER, \"X\" NUMBER)".into()),
            ..view.clone()
        };
        let plan = plan_deploy(&[table], db, db, DeployOptions::default());
        assert!(!plan.steps[0].runnable);
        assert!(plan.steps[0].sql.contains("MANUAL REVIEW"));
        assert!(
            plan.steps[0]
                .sql
                .contains("-- CREATE TABLE \"T\" (\"ID\" NUMBER, \"X\" NUMBER)")
        );
        let sequence = ComparedDifference {
            object: obj(
                "SQLLY",
                "SEQ",
                "SEQUENCE",
                "CREATE SEQUENCE \"SEQ\" START WITH 1",
            ),
            target_definition: Some("CREATE SEQUENCE \"SEQ\" START WITH 1 CACHE 50".into()),
            ..view
        };
        let plan = plan_deploy(&[sequence], db, db, DeployOptions::default());
        assert!(!plan.steps[0].runnable);
        assert!(plan.steps[0].sql.contains("MANUAL REVIEW"));
    }

    #[test]
    fn oracle_missing_objects_deploy_in_dependency_order() {
        let db = DatabaseType::Oracle;
        let source = vec![
            obj(
                "SQLLY",
                "T.T_FK",
                "FOREIGN KEY",
                "ALTER TABLE \"T\" ADD CONSTRAINT \"T_FK\" FOREIGN KEY (\"P\") REFERENCES \"P\" (\"ID\")",
            ),
            obj(
                "SQLLY",
                "V",
                "VIEW",
                "CREATE OR REPLACE FORCE EDITIONABLE VIEW \"V\" AS SELECT 1 FROM DUAL",
            ),
            obj("SQLLY", "T", "TABLE", "CREATE TABLE \"T\" (\"ID\" NUMBER)"),
            obj(
                "SQLLY",
                "SEQ",
                "SEQUENCE",
                "CREATE SEQUENCE \"SEQ\" START WITH 1",
            ),
            obj(
                "SQLLY",
                "T.(PRIMARY KEY \"ID\")",
                "PRIMARY KEY",
                "ALTER TABLE \"T\" ADD PRIMARY KEY (\"ID\")",
            ),
            obj(
                "SQLLY",
                "T.T_IX",
                "INDEX",
                "CREATE INDEX \"T_IX\" ON \"T\" (\"ID\")",
            ),
            obj(
                "SQLLY",
                "TY",
                "TYPE",
                "CREATE OR REPLACE EDITIONABLE TYPE \"TY\" AS OBJECT (a NUMBER);",
            ),
            obj(
                "SQLLY",
                "SYN",
                "SYNONYM",
                "CREATE OR REPLACE EDITIONABLE SYNONYM \"SYN\" FOR \"T\"",
            ),
            obj(
                "SQLLY",
                "MV",
                "MATERIALIZED VIEW",
                "CREATE MATERIALIZED VIEW \"MV\" AS SELECT 1 FROM DUAL",
            ),
            obj(
                "SQLLY",
                "T.TRG",
                "TRIGGER",
                "CREATE OR REPLACE EDITIONABLE TRIGGER \"TRG\" BEFORE INSERT ON t FOR EACH ROW BEGIN NULL; END;\nALTER TRIGGER \"TRG\" ENABLE",
            ),
            obj(
                "SQLLY",
                "P",
                "ROUTINE",
                "CREATE OR REPLACE EDITIONABLE PROCEDURE \"P\" AS BEGIN NULL; END;",
            ),
        ];
        let diffs = compare_objects(&source, &[], db, db);
        assert!(
            diffs
                .iter()
                .all(|d| d.status == DifferenceStatus::MissingInTarget)
        );
        let plan = plan_deploy(&diffs, db, db, DeployOptions::default());
        let kinds: Vec<&str> = plan.steps.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "SEQUENCE",
                "TYPE",
                "TABLE",
                "PRIMARY KEY",
                "INDEX",
                "FOREIGN KEY",
                "VIEW",
                "MATERIALIZED VIEW",
                "SYNONYM",
                "ROUTINE",
                "TRIGGER",
            ]
        );
        assert!(plan.steps.iter().all(|s| s.runnable), "{kinds:?}");
        let script = deploy_script(&plan, "");
        assert!(script.contains("ALTER TABLE \"T\" ADD PRIMARY KEY (\"ID\")"));
        // Rollback drops what was created, in reverse, with Oracle syntax.
        let rollback = rollback_script(&plan, "");
        assert!(rollback.contains("DROP TABLE \"SQLLY\".\"T\" -- data loss if executed"));
    }

    #[test]
    fn oracle_drop_and_rename_statements_use_double_quotes() {
        let db = DatabaseType::Oracle;
        let drop = |name: &str, kind: &str, def: &str| {
            let target = vec![obj("SQLLY", name, kind, def)];
            let diffs = compare_objects(&[], &target, db, db);
            let plan = plan_deploy(
                &diffs,
                db,
                db,
                DeployOptions {
                    include_drops: true,
                    ..DeployOptions::default()
                },
            );
            plan.steps[0].sql.clone()
        };
        assert_eq!(
            drop("T", "TABLE", "CREATE TABLE \"T\" (\"ID\" NUMBER)"),
            "DROP TABLE \"SQLLY\".\"T\";"
        );
        assert_eq!(
            drop(
                "V",
                "VIEW",
                "CREATE OR REPLACE VIEW \"V\" AS SELECT 1 FROM DUAL"
            ),
            "DROP VIEW \"SQLLY\".\"V\";"
        );
        assert_eq!(
            drop("T.T_IX", "INDEX", "CREATE INDEX \"T_IX\" ON \"T\" (\"ID\")"),
            "DROP INDEX \"SQLLY\".\"T_IX\";"
        );
        assert_eq!(
            drop("SEQ", "SEQUENCE", "CREATE SEQUENCE \"SEQ\""),
            "DROP SEQUENCE \"SQLLY\".\"SEQ\";"
        );
        assert_eq!(
            drop(
                "SYN",
                "SYNONYM",
                "CREATE OR REPLACE SYNONYM \"SYN\" FOR \"T\""
            ),
            "DROP SYNONYM \"SQLLY\".\"SYN\";"
        );
        assert_eq!(
            drop(
                "MV",
                "MATERIALIZED VIEW",
                "CREATE MATERIALIZED VIEW \"MV\" AS SELECT 1 FROM DUAL"
            ),
            "DROP MATERIALIZED VIEW \"SQLLY\".\"MV\";"
        );
        assert_eq!(
            drop(
                "T.TRG",
                "TRIGGER",
                "CREATE OR REPLACE TRIGGER \"TRG\" BEFORE INSERT ON t FOR EACH ROW BEGIN NULL; END;"
            ),
            "DROP TRIGGER \"SQLLY\".\"TRG\";"
        );
        assert_eq!(
            drop(
                "T.T_FK",
                "FOREIGN KEY",
                "ALTER TABLE \"T\" ADD CONSTRAINT \"T_FK\" FOREIGN KEY (\"P\") REFERENCES \"P\" (\"ID\")"
            ),
            "ALTER TABLE \"SQLLY\".\"T\" DROP CONSTRAINT \"T_FK\";"
        );
        assert_eq!(
            drop(
                "T.T_UQ",
                "UNIQUE CONSTRAINT",
                "ALTER TABLE \"T\" ADD CONSTRAINT \"T_UQ\" UNIQUE (\"A\")"
            ),
            "ALTER TABLE \"SQLLY\".\"T\" DROP CONSTRAINT \"T_UQ\";"
        );
        // A system-generated constraint name is not addressable.
        assert!(
            drop(
                "T.(PRIMARY KEY \"ID\")",
                "PRIMARY KEY",
                "ALTER TABLE \"T\" ADD PRIMARY KEY (\"ID\")"
            )
            .starts_with("-- MANUAL REVIEW: DROP PRIMARY KEY")
        );
        assert_eq!(
            drop(
                "P",
                "ROUTINE",
                "CREATE OR REPLACE EDITIONABLE PROCEDURE \"P\" AS BEGIN NULL; END;"
            ),
            "DROP PROCEDURE \"SQLLY\".\"P\";"
        );
        assert_eq!(
            drop(
                "F",
                "ROUTINE",
                "CREATE OR REPLACE NONEDITIONABLE FUNCTION \"F\" RETURN NUMBER AS BEGIN RETURN 1; END;"
            ),
            "DROP FUNCTION \"SQLLY\".\"F\";"
        );
        assert_eq!(
            drop(
                "PKG",
                "ROUTINE",
                "CREATE OR REPLACE EDITIONABLE PACKAGE \"PKG\" AS END;"
            ),
            "DROP PACKAGE \"SQLLY\".\"PKG\";"
        );
        assert_eq!(
            drop(
                "PKG (BODY)",
                "ROUTINE",
                "CREATE OR REPLACE EDITIONABLE PACKAGE BODY \"PKG\" AS END;"
            ),
            "DROP PACKAGE BODY \"SQLLY\".\"PKG\";"
        );
        assert_eq!(
            drop(
                "TY",
                "TYPE",
                "CREATE OR REPLACE EDITIONABLE TYPE \"TY\" AS OBJECT (a NUMBER);"
            ),
            "DROP TYPE \"SQLLY\".\"TY\";"
        );
        assert_eq!(
            drop(
                "TY (BODY)",
                "TYPE",
                "CREATE OR REPLACE TYPE BODY \"TY\" AS END;"
            ),
            "DROP TYPE BODY \"SQLLY\".\"TY\";"
        );
        assert!(
            drop("X", "ROUTINE", "-- unreadable").starts_with("-- MANUAL REVIEW: DROP ROUTINE")
        );
        assert_eq!(oracle_quote("we\"ird"), "\"we\"\"ird\"");

        let renamed = ComparedDifference {
            status: DifferenceStatus::Renamed { from: "OLD".into() },
            object: obj(
                "SQLLY",
                "NEW",
                "TABLE",
                "CREATE TABLE \"NEW\" (\"ID\" NUMBER)",
            ),
            target_definition: Some("CREATE TABLE \"NEW\" (\"ID\" NUMBER)".into()),
            note: String::new(),
        };
        let plan = plan_deploy(
            std::slice::from_ref(&renamed),
            db,
            db,
            DeployOptions::default(),
        );
        assert_eq!(
            plan.steps[0].sql,
            "ALTER TABLE \"SQLLY\".\"OLD\" RENAME TO \"NEW\";"
        );
        assert_eq!(
            plan.steps[0].rollback_sql,
            "ALTER TABLE \"SQLLY\".\"NEW\" RENAME TO \"OLD\";"
        );
        let view = ComparedDifference {
            object: obj(
                "SQLLY",
                "NEW",
                "VIEW",
                "CREATE OR REPLACE VIEW \"NEW\" AS SELECT 1 FROM DUAL",
            ),
            target_definition: Some("CREATE OR REPLACE VIEW \"NEW\" AS SELECT 1 FROM DUAL".into()),
            ..renamed.clone()
        };
        let plan = plan_deploy(&[view], db, db, DeployOptions::default());
        assert_eq!(plan.steps[0].sql, "RENAME \"OLD\" TO \"NEW\";");
        let routine = ComparedDifference {
            object: obj(
                "SQLLY",
                "NEW",
                "ROUTINE",
                "CREATE OR REPLACE PROCEDURE \"NEW\" AS BEGIN NULL; END;",
            ),
            target_definition: Some(
                "CREATE OR REPLACE PROCEDURE \"NEW\" AS BEGIN NULL; END;".into(),
            ),
            ..renamed
        };
        let plan = plan_deploy(&[routine], db, db, DeployOptions::default());
        assert!(
            plan.steps[0]
                .sql
                .starts_with("-- Manual review: rename SQLLY.OLD -> NEW")
        );
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
    fn cross_engine_changes_require_manual_review_and_never_emit_source_sql() {
        let source = vec![obj(
            "public",
            "v1",
            "VIEW",
            "CREATE VIEW public.v1 AS SELECT 1",
        )];
        let target = vec![obj(
            "public",
            "v1",
            "VIEW",
            "CREATE VIEW `public`.`v1` AS SELECT 2",
        )];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::Postgres,
            DatabaseType::MySql,
        );
        assert_eq!(diffs[0].status, DifferenceStatus::ManualReview);
        let script = script_differences(&diffs, DatabaseType::Postgres, DatabaseType::MySql);
        assert!(script.contains("-- CREATE VIEW public.v1 AS SELECT 1"));
        assert!(!script.lines().any(|line| line.starts_with("CREATE VIEW")));
        assert!(!script.to_ascii_uppercase().contains("DROP VIEW"));
    }

    #[test]
    fn target_only_objects_are_never_scripted_for_drop() {
        let target = vec![obj("dbo", "old_table", "TABLE", "id int")];
        let diffs = compare_objects(
            &[],
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let script = script_differences(&diffs, DatabaseType::SqlServer, DatabaseType::SqlServer);
        assert_eq!(diffs[0].status, DifferenceStatus::ExtraInTarget);
        assert!(script.contains("No DROP statement is generated"));
        assert!(!script.to_ascii_uppercase().contains("DROP TABLE"));
    }

    #[test]
    fn rename_detected_for_same_columns_different_name() {
        let cols = "id int(4) NOT NULL, name nvarchar(50) NULL";
        let source = vec![obj("dbo", "customers", "TABLE", cols)];
        let target = vec![obj("dbo", "customer", "TABLE", cols)];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let candidates = detect_renames(&diffs);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].similarity_percent, 100);
    }

    #[test]
    fn rename_below_threshold_is_not_suggested() {
        let source = vec![obj("dbo", "a", "TABLE", "a, b, c")];
        let target = vec![obj("dbo", "b", "TABLE", "x, y, z, p, q, r, s")];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        assert!(detect_renames(&diffs).is_empty());
    }

    #[test]
    fn rename_never_pairs_across_kinds_or_schemas() {
        let source = vec![obj("dbo", "t1", "TABLE", "id int")];
        let target = vec![obj("dbo", "t1", "VIEW", "id int")];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        assert!(detect_renames(&diffs).is_empty());
        let source = vec![obj("dbo", "t1", "TABLE", "id int")];
        let target = vec![obj("sales", "t1", "TABLE", "id int")];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        assert!(detect_renames(&diffs).is_empty());
    }

    #[test]
    fn rename_pairs_each_object_once() {
        let source = vec![
            obj("dbo", "a2", "TABLE", "id int"),
            obj("dbo", "b2", "TABLE", "id int"),
        ];
        let target = vec![
            obj("dbo", "a1", "TABLE", "id int"),
            obj("dbo", "b1", "TABLE", "id int"),
        ];
        let diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let candidates = detect_renames(&diffs);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].from_name, "a1");
        assert_eq!(candidates[1].from_name, "b1");
    }

    #[test]
    fn apply_renames_collapses_two_rows_into_one() {
        let source = vec![obj("dbo", "customers", "TABLE", "id int")];
        let target = vec![obj("dbo", "customer", "TABLE", "id int")];
        let mut diffs = compare_objects(
            &source,
            &target,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let candidates = detect_renames(&diffs);
        apply_renames(&mut diffs, &candidates);
        assert_eq!(diffs.len(), 1);
        assert!(matches!(diffs[0].status, DifferenceStatus::Renamed { .. }));
    }

    #[test]
    fn renamed_table_scripts_sp_rename_on_sql_server() {
        let mut diffs = vec![ComparedDifference {
            status: DifferenceStatus::Renamed {
                from: "customer".into(),
            },
            object: obj("dbo", "customers", "TABLE", "id int"),
            target_definition: Some("id int".into()),
            note: String::new(),
        }];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        assert!(plan.steps[0].sql.contains("sp_rename"));
        diffs[0].object.schema = "public".into();
        diffs[0].object.name = "customers".into();
        diffs[0].status = DifferenceStatus::Renamed {
            from: "customer".into(),
        };
        let plan = plan_deploy(
            &diffs,
            DatabaseType::Postgres,
            DatabaseType::Postgres,
            DeployOptions::default(),
        );
        assert!(plan.steps[0].sql.contains("ALTER TABLE"));
    }

    #[test]
    fn deploy_orders_tables_before_foreign_keys_and_views() {
        let diffs = vec![
            ComparedDifference {
                status: DifferenceStatus::MissingInTarget,
                object: obj(
                    "dbo",
                    "Orders.FK_Orders_Customer",
                    "FOREIGN KEY",
                    "-- (CustomerId) references dbo.Customers (Id)",
                ),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::MissingInTarget,
                object: obj("dbo", "Customers", "TABLE", "Id int NOT NULL"),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::MissingInTarget,
                object: obj(
                    "dbo",
                    "vOrders",
                    "VIEW",
                    "CREATE VIEW dbo.vOrders AS SELECT 1 FROM dbo.Customers",
                ),
                target_definition: None,
                note: String::new(),
            },
        ];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        let table_idx = plan.steps.iter().position(|s| s.kind == "TABLE").unwrap();
        let fk_idx = plan
            .steps
            .iter()
            .position(|s| s.kind == "FOREIGN KEY")
            .unwrap();
        let view_idx = plan.steps.iter().position(|s| s.kind == "VIEW").unwrap();
        assert!(table_idx < fk_idx);
        assert!(table_idx < view_idx);
    }

    #[test]
    fn drops_are_reverse_ordered_and_opt_in() {
        let diffs = vec![
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj("dbo", "OldTable", "TABLE", "id int"),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj(
                    "dbo",
                    "OldTrigger",
                    "TRIGGER",
                    "CREATE TRIGGER t AS SELECT 1",
                ),
                target_definition: None,
                note: String::new(),
            },
        ];
        let off = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        assert!(
            !off.steps
                .iter()
                .any(|s| { s.runnable && s.sql.to_ascii_uppercase().contains("DROP ") })
        );
        let on = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions {
                include_drops: true,
                ..Default::default()
            },
        );
        let trigger_idx = on.steps.iter().position(|s| s.kind == "TRIGGER").unwrap();
        let table_idx = on.steps.iter().position(|s| s.kind == "TABLE").unwrap();
        assert!(trigger_idx < table_idx);
    }

    #[test]
    fn changed_view_uses_create_or_alter_on_sql_server() {
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::Changed,
            object: obj("dbo", "v1", "VIEW", "CREATE VIEW dbo.v1 AS SELECT 2"),
            target_definition: Some("CREATE VIEW dbo.v1 AS SELECT 1".into()),
            note: String::new(),
        }];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        assert!(
            plan.steps[0]
                .sql
                .to_ascii_uppercase()
                .contains("CREATE OR ALTER")
        );
    }

    fn changed_sql(kind: &str, source: &str, target: &str, db: DatabaseType) -> (String, String) {
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::Changed,
            object: obj("dbo", "x", kind, source),
            target_definition: Some(target.into()),
            note: String::new(),
        }];
        let plan = plan_deploy(&diffs, db, db, DeployOptions::default());
        let step = &plan.steps[0];
        (step.sql.clone(), step.rollback_sql.clone())
    }

    #[test]
    fn changed_definitions_keep_header_comments_and_object_keyword() {
        // The header comment above CREATE and the object keyword after it
        // both survive; OR ALTER is spliced in right after CREATE.
        let (sql, rollback) = changed_sql(
            "VIEW",
            "-- Orders by customer\n/* owner: sales */\nCREATE VIEW dbo.vOrders AS SELECT 2 AS x",
            "-- old header\nCREATE VIEW dbo.vOrders AS SELECT 1 AS x",
            DatabaseType::SqlServer,
        );
        assert_eq!(
            sql,
            "-- Orders by customer\n/* owner: sales */\nCREATE OR ALTER VIEW dbo.vOrders AS SELECT 2 AS x"
        );
        assert_eq!(
            rollback,
            "-- old header\nCREATE OR ALTER VIEW dbo.vOrders AS SELECT 1 AS x"
        );

        // Extra whitespace between CREATE and the keyword (SQL Server stores
        // a deployed `CREATE OR ALTER` as `CREATE   VIEW`) collapses to one
        // space, so repeated deploys do not pile up spaces.
        let (sql, _) = changed_sql(
            "ROUTINE",
            "CREATE  PROCEDURE dbo.p AS SELECT 1",
            "CREATE  PROCEDURE dbo.p AS SELECT 0",
            DatabaseType::SqlServer,
        );
        assert_eq!(sql, "CREATE OR ALTER PROCEDURE dbo.p AS SELECT 1");
        let (sql, _) = changed_sql(
            "VIEW",
            "CREATE   VIEW dbo.v AS SELECT 1",
            "CREATE VIEW dbo.v AS SELECT 0",
            DatabaseType::SqlServer,
        );
        assert_eq!(sql, "CREATE OR ALTER VIEW dbo.v AS SELECT 1");
        // A comment between CREATE and the keyword is kept.
        let (sql, _) = changed_sql(
            "VIEW",
            "CREATE /* v2 */ VIEW dbo.v AS SELECT 1",
            "CREATE VIEW dbo.v AS SELECT 0",
            DatabaseType::SqlServer,
        );
        assert_eq!(sql, "CREATE OR ALTER /* v2 */ VIEW dbo.v AS SELECT 1");

        // A CREATE VIEW inside the body (dynamic SQL, a comment, a string)
        // is not the statement's head.
        let body = "/* builds CREATE VIEW dbo.tmp */\nCREATE PROCEDURE dbo.build AS\nBEGIN\n  EXEC (N'CREATE VIEW dbo.tmp AS SELECT 1');\nEND";
        let (sql, _) = changed_sql("ROUTINE", body, body, DatabaseType::SqlServer);
        assert_eq!(
            sql,
            "/* builds CREATE VIEW dbo.tmp */\nCREATE OR ALTER PROCEDURE dbo.build AS\nBEGIN\n  EXEC (N'CREATE VIEW dbo.tmp AS SELECT 1');\nEND"
        );

        // A definition already in the OR ALTER form is left alone.
        let (sql, _) = changed_sql(
            "VIEW",
            "CREATE OR ALTER VIEW dbo.v AS SELECT 1",
            "CREATE VIEW dbo.v AS SELECT 0",
            DatabaseType::SqlServer,
        );
        assert_eq!(sql, "CREATE OR ALTER VIEW dbo.v AS SELECT 1");

        // PostgreSQL gets OR REPLACE the same way.
        let (sql, rollback) = changed_sql(
            "VIEW",
            "-- note\nCREATE VIEW public.v AS SELECT 2",
            "CREATE OR REPLACE VIEW public.v AS SELECT 1",
            DatabaseType::Postgres,
        );
        assert_eq!(sql, "-- note\nCREATE OR REPLACE VIEW public.v AS SELECT 2");
        assert_eq!(rollback, "CREATE OR REPLACE VIEW public.v AS SELECT 1");
    }

    #[test]
    fn clickhouse_supports_its_own_object_kinds() {
        for kind in [
            "TABLE",
            "VIEW",
            "MATERIALIZED VIEW",
            "DICTIONARY",
            "INDEX",
            "PRIMARY KEY",
            "CHECK CONSTRAINT",
        ] {
            assert!(supports_kind(DatabaseType::ClickHouse, kind), "{kind}");
        }
        for kind in ["ROUTINE", "TRIGGER", "FOREIGN KEY", "UNIQUE CONSTRAINT"] {
            assert!(!supports_kind(DatabaseType::ClickHouse, kind), "{kind}");
        }
    }

    #[test]
    fn clickhouse_changed_view_and_dictionary_use_create_or_replace() {
        let diffs = vec![
            ComparedDifference {
                status: DifferenceStatus::Changed,
                object: obj("shop", "v1", "VIEW", "CREATE VIEW shop.v1 AS SELECT 2"),
                target_definition: Some("CREATE VIEW shop.v1 AS SELECT 1".into()),
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::Changed,
                object: obj(
                    "shop",
                    "d1",
                    "DICTIONARY",
                    "CREATE DICTIONARY shop.d1 (id UInt64) PRIMARY KEY id SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)",
                ),
                target_definition: Some("CREATE DICTIONARY shop.d1 (id UInt32) PRIMARY KEY id SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)".into()),
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::Changed,
                object: obj(
                    "shop",
                    "mv",
                    "MATERIALIZED VIEW",
                    "CREATE MATERIALIZED VIEW shop.mv ENGINE = MergeTree ORDER BY id AS SELECT id FROM shop.t",
                ),
                target_definition: Some("CREATE MATERIALIZED VIEW shop.mv ENGINE = MergeTree ORDER BY id AS SELECT id, 1 FROM shop.t".into()),
                note: String::new(),
            },
        ];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::ClickHouse,
            DatabaseType::ClickHouse,
            DeployOptions::default(),
        );
        let view = plan.steps.iter().find(|s| s.kind == "VIEW").unwrap();
        assert_eq!(view.sql, "CREATE OR REPLACE VIEW shop.v1 AS SELECT 2");
        assert_eq!(
            view.rollback_sql,
            "CREATE OR REPLACE VIEW shop.v1 AS SELECT 1"
        );
        assert!(view.runnable);
        let dictionary = plan.steps.iter().find(|s| s.kind == "DICTIONARY").unwrap();
        assert!(
            dictionary
                .sql
                .starts_with("CREATE OR REPLACE DICTIONARY shop.d1")
        );
        let mv = plan
            .steps
            .iter()
            .find(|s| s.kind == "MATERIALIZED VIEW")
            .unwrap();
        assert!(
            mv.sql
                .starts_with("DROP VIEW IF EXISTS `shop`.`mv`;\nCREATE MATERIALIZED VIEW")
        );
        assert!(mv.runnable);
        assert!(plan.warnings.iter().any(|w| w.contains("inner table")));
    }

    #[test]
    fn clickhouse_renames_and_drops_use_its_verbs() {
        let table = obj("shop", "orders_v2", "TABLE", "id UInt32");
        let (forward, back) = rename_sql("orders", &table, DatabaseType::ClickHouse);
        assert_eq!(
            forward,
            "RENAME TABLE `shop`.`orders` TO `shop`.`orders_v2`;"
        );
        assert_eq!(back, "RENAME TABLE `shop`.`orders_v2` TO `shop`.`orders`;");
        let dictionary = obj("shop", "d2", "DICTIONARY", "");
        assert_eq!(
            rename_sql("d1", &dictionary, DatabaseType::ClickHouse).0,
            "RENAME DICTIONARY `shop`.`d1` TO `shop`.`d2`;"
        );
        let index = obj("shop", "orders.idx_name", "INDEX", "");
        assert!(
            rename_sql("orders.idx_old", &index, DatabaseType::ClickHouse)
                .0
                .starts_with("-- Manual review")
        );
        assert_eq!(
            drop_sql(&table, DatabaseType::ClickHouse),
            "DROP TABLE `shop`.`orders_v2`;"
        );
        assert_eq!(
            drop_sql(&index, DatabaseType::ClickHouse),
            "ALTER TABLE `shop`.`orders` DROP INDEX `idx_name`;"
        );
        assert_eq!(
            drop_sql(&obj("shop", "v", "VIEW", ""), DatabaseType::ClickHouse),
            "DROP VIEW `shop`.`v`;"
        );
        assert_eq!(
            drop_sql(
                &obj("shop", "mv", "MATERIALIZED VIEW", ""),
                DatabaseType::ClickHouse
            ),
            "DROP VIEW `shop`.`mv`;"
        );
        assert_eq!(
            drop_sql(&dictionary, DatabaseType::ClickHouse),
            "DROP DICTIONARY `shop`.`d2`;"
        );
        assert!(
            drop_sql(
                &obj("shop", "t.pk", "PRIMARY KEY", ""),
                DatabaseType::ClickHouse
            )
            .starts_with("-- MANUAL REVIEW")
        );
    }

    #[test]
    fn changed_view_uses_create_or_replace_on_postgres() {
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::Changed,
            object: obj("public", "v1", "VIEW", "CREATE VIEW public.v1 AS SELECT 2"),
            target_definition: Some("CREATE VIEW public.v1 AS SELECT 1".into()),
            note: String::new(),
        }];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::Postgres,
            DatabaseType::Postgres,
            DeployOptions::default(),
        );
        assert!(
            plan.steps[0]
                .sql
                .to_ascii_uppercase()
                .contains("CREATE OR REPLACE")
        );
    }

    #[test]
    fn duckdb_plans_replace_views_rename_in_place_and_refuse_constraint_drops() {
        let db = DatabaseType::DuckDb;
        let diffs = vec![
            ComparedDifference {
                status: DifferenceStatus::Changed,
                object: obj("main", "v1", "VIEW", "CREATE VIEW v1 AS SELECT 2"),
                target_definition: Some("CREATE VIEW v1 AS SELECT 1".into()),
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::Changed,
                object: obj("main", "add1", "ROUTINE", "CREATE MACRO add1(a) AS a + 2"),
                target_definition: Some("CREATE MACRO add1(a) AS a + 1".into()),
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::Renamed {
                    from: "customer".into(),
                },
                object: obj(
                    "main",
                    "customers",
                    "TABLE",
                    "CREATE TABLE customers(id INTEGER)",
                ),
                target_definition: Some("CREATE TABLE customers(id INTEGER)".into()),
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj(
                    "main",
                    "orders.orders_customer_id_id_fkey",
                    "FOREIGN KEY",
                    "FOREIGN KEY (customer_id) REFERENCES customer(id)",
                ),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj(
                    "main",
                    "orders.ix_orders",
                    "INDEX",
                    "CREATE INDEX ix_orders ON orders(x);",
                ),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::ExtraInTarget,
                object: obj("main", "old", "TABLE", "CREATE TABLE old(id INTEGER);"),
                target_definition: None,
                note: String::new(),
            },
        ];
        let plan = plan_deploy(
            &diffs,
            db,
            db,
            DeployOptions {
                include_drops: true,
                ..Default::default()
            },
        );
        let step = |identity: &str| plan.steps.iter().find(|s| s.identity == identity).unwrap();
        assert_eq!(step("main.v1").sql, "CREATE OR REPLACE VIEW v1 AS SELECT 2");
        assert!(step("main.v1").runnable);
        assert_eq!(
            step("main.add1").sql,
            "CREATE OR REPLACE MACRO add1(a) AS a + 2"
        );
        assert_eq!(
            step("main.customers").sql,
            "ALTER TABLE \"main\".\"customer\" RENAME TO \"customers\";"
        );
        assert_eq!(
            step("main.customers").rollback_sql,
            "ALTER TABLE \"main\".\"customers\" RENAME TO \"customer\";"
        );
        assert_eq!(
            step("main.orders.ix_orders").sql,
            "DROP INDEX \"main\".\"ix_orders\";"
        );
        assert_eq!(step("main.old").sql, "DROP TABLE \"main\".\"old\";");
        let fk = step("main.orders.orders_customer_id_id_fkey");
        assert!(fk.sql.contains("MANUAL REVIEW"), "{}", fk.sql);
        assert!(fk.sql.contains("no DROP CONSTRAINT"), "{}", fk.sql);
        assert!(supports_kind(db, "SEQUENCE"));
        assert!(supports_kind(db, "ROUTINE"));
        assert!(!supports_kind(db, "TRIGGER"));
    }

    #[test]
    fn changed_table_columns_are_manual_review() {
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::Changed,
            object: obj("dbo", "t1", "TABLE", "id int, name nvarchar(10)"),
            target_definition: Some("id int".into()),
            note: String::new(),
        }];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        assert!(!plan.steps[0].runnable);
        assert!(plan.steps[0].sql.contains("MANUAL REVIEW"));
    }

    #[test]
    fn rollback_is_reverse_of_deploy() {
        let diffs = vec![
            ComparedDifference {
                status: DifferenceStatus::MissingInTarget,
                object: obj("dbo", "t1", "TABLE", "CREATE TABLE dbo.t1(id int)"),
                target_definition: None,
                note: String::new(),
            },
            ComparedDifference {
                status: DifferenceStatus::MissingInTarget,
                object: obj("dbo", "t2", "TABLE", "CREATE TABLE dbo.t2(id int)"),
                target_definition: None,
                note: String::new(),
            },
        ];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        let deploy = deploy_script(&plan, "");
        let rollback = rollback_script(&plan, "");
        assert!(rollback.contains("t2"));
        assert!(deploy.find("t1").unwrap() < deploy.find("t2").unwrap());
        assert!(rollback.find("t2").unwrap() < rollback.find("t1").unwrap());
    }

    #[test]
    fn rollback_for_created_object_is_drop() {
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::MissingInTarget,
            object: obj("dbo", "t1", "TABLE", "CREATE TABLE dbo.t1(id int)"),
            target_definition: None,
            note: String::new(),
        }];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        assert!(
            plan.steps[0]
                .rollback_sql
                .to_ascii_uppercase()
                .contains("DROP TABLE")
        );
    }

    #[test]
    fn deploy_script_never_contains_go() {
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::MissingInTarget,
            object: obj("dbo", "t1", "TABLE", "CREATE TABLE dbo.t1(id int)"),
            target_definition: None,
            note: String::new(),
        }];
        let plan = plan_deploy(
            &diffs,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        let script = deploy_script(&plan, "");
        assert!(!script.lines().any(|line| line == "GO"));
    }

    fn postgres_trigger_plan(target_postgres_major: Option<u32>) -> DeployPlan {
        let db = DatabaseType::Postgres;
        let diffs = vec![ComparedDifference {
            status: DifferenceStatus::Changed,
            object: obj(
                "public",
                "orders.audit",
                "TRIGGER",
                "CREATE TRIGGER audit AFTER INSERT ON orders FOR EACH ROW EXECUTE FUNCTION log()",
            ),
            target_definition: Some(
                "CREATE TRIGGER audit AFTER UPDATE ON orders FOR EACH ROW EXECUTE FUNCTION log()"
                    .into(),
            ),
            note: String::new(),
        }];
        plan_deploy(
            &diffs,
            db,
            db,
            DeployOptions {
                target_postgres_major,
                ..DeployOptions::default()
            },
        )
    }

    #[test]
    fn postgres_trigger_is_replaced_in_place_on_14_and_later() {
        for major in [14, 16] {
            let plan = postgres_trigger_plan(Some(major));
            assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
            assert!(plan.steps[0].runnable);
            assert!(
                plan.steps[0].sql.starts_with("CREATE OR REPLACE TRIGGER"),
                "{}",
                plan.steps[0].sql
            );
        }
    }

    #[test]
    fn postgres_trigger_warns_firmly_before_14() {
        let plan = postgres_trigger_plan(Some(13));
        assert!(!plan.steps[0].runnable);
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(
            plan.warnings[0].key(),
            Some("services.schema_compare.warnings.postgres_trigger")
        );
    }

    #[test]
    fn postgres_trigger_warns_softly_when_the_version_is_unknown() {
        let plan = postgres_trigger_plan(None);
        assert!(!plan.steps[0].runnable);
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(
            plan.warnings[0].key(),
            Some("services.schema_compare.warnings.postgres_trigger_unknown_version")
        );
    }
}
