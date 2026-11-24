//! Local data settings on cells and notebooks.
//!
//! A SQL cell targets the notebook's own local database (lane D's DuckDB
//! workspace) instead of a connection with `spec.target = "local"`; it keeps
//! kind `sql`, so older builds still open it as a SQL cell. Such a cell is
//! [`ExecClass::Local`](super::super::ExecClass::Local) everywhere the core
//! asks [`Cell::exec_class`], and every `{{x}}` it reads is materialized into
//! the workspace rather than inlined.
//!
//! Other keys:
//!
//! - `spec.materialize_as`: the relation a cell's output is copied into
//!   ("Materialize locally as …"); [`local_relation`] falls back to the
//!   cell's relation name.
//! - `spec.cache_storage`: where a cell's cached output is kept
//!   ([`CacheSettings`]); the TTL and pinning live in the core's
//!   `execution_policy.cache`.
//! - `settings.local_auto_materialize` (notebook): copy cross-connection
//!   inputs into local data without asking each time.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::super::document::{Cell, CellKind, NotebookDoc};

pub const TARGET_KEY: &str = "target";
pub const TARGET_LOCAL: &str = "local";
/// The connection slot id that also means "the notebook's local data".
pub const LOCAL_SLOT: &str = "local";
pub const MATERIALIZE_AS_KEY: &str = "materialize_as";
pub const CACHE_STORAGE_KEY: &str = "cache_storage";
pub const AUTO_MATERIALIZE_KEY: &str = "local_auto_materialize";

/// Longest relation name the local database accepts from a cell name.
const MAX_RELATION_LEN: usize = 63;

/// Whether a SQL cell runs against the notebook's local data: its spec says
/// so, or it names the reserved `local` connection slot (how the command-line
/// runner spells the same thing).
pub fn targets_local(cell: &Cell) -> bool {
    cell.kind == CellKind::Sql
        && (cell.spec.get(TARGET_KEY).and_then(Value::as_str) == Some(TARGET_LOCAL)
            || cell.connection_ref.as_deref() == Some(LOCAL_SLOT))
}

fn spec_object(cell: &mut Cell) -> &mut Map<String, Value> {
    if !cell.spec.is_object() {
        cell.spec = Value::Object(Map::new());
    }
    cell.spec.as_object_mut().expect("spec is an object")
}

/// Drop `spec` back to null when the last key went, so plain cells
/// serialize without an empty object.
fn tidy(cell: &mut Cell) {
    if cell.spec.as_object().is_some_and(Map::is_empty) {
        cell.spec = Value::Null;
    }
}

/// Point a SQL cell at local data (or back at its connection). Returns
/// whether anything changed.
pub fn set_targets_local(cell: &mut Cell, local: bool) -> bool {
    if cell.kind != CellKind::Sql || targets_local(cell) == local {
        return false;
    }
    if !local && cell.connection_ref.as_deref() == Some(LOCAL_SLOT) {
        cell.connection_ref = None;
    }
    let spec = spec_object(cell);
    if local {
        spec.insert(TARGET_KEY.into(), Value::String(TARGET_LOCAL.into()));
    } else {
        spec.remove(TARGET_KEY);
    }
    tidy(cell);
    true
}

/// Rewrite the command line's older spelling of a local cell (naming the
/// reserved `local` connection slot) as `spec.target = "local"`, and drop
/// a declared `local` slot no cell names any more. Returns how many cells
/// changed. Run on every load, so the spec spelling is the only one saved.
pub fn canonicalize_local_targets(doc: &mut NotebookDoc) -> usize {
    let mut changed = 0;
    for cell in &mut doc.cells {
        if cell.kind == CellKind::Sql && cell.connection_ref.as_deref() == Some(LOCAL_SLOT) {
            cell.connection_ref = None;
            spec_object(cell).insert(TARGET_KEY.into(), Value::String(TARGET_LOCAL.into()));
            changed += 1;
        }
    }
    let named = doc
        .cells
        .iter()
        .any(|c| c.connection_ref.as_deref() == Some(LOCAL_SLOT));
    if !named {
        doc.connection_refs.retain(|r| r.id != LOCAL_SLOT);
    }
    changed
}

/// The relation name the user chose for the cell's local copy.
pub fn materialized_as(cell: &Cell) -> Option<&str> {
    cell.spec
        .get(MATERIALIZE_AS_KEY)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

pub fn set_materialized_as(cell: &mut Cell, relation: Option<&str>) -> bool {
    let relation = relation.map(str::trim).filter(|s| !s.is_empty());
    if materialized_as(cell) == relation {
        return false;
    }
    let spec = spec_object(cell);
    match relation {
        Some(name) => {
            spec.insert(MATERIALIZE_AS_KEY.into(), Value::String(name.to_string()));
        }
        None => {
            spec.remove(MATERIALIZE_AS_KEY);
        }
    }
    tidy(cell);
    true
}

/// The relation holding a cell's output in local data: the chosen name
/// (an attachment's import table), else the cell's own relation name.
/// Local cells read `{{x}}` from here.
pub fn local_relation(cell: &Cell) -> String {
    let imported = (cell.kind == CellKind::Attachment)
        .then(|| super::attachment::AttachmentSpec::of(cell).relation)
        .flatten();
    materialized_as(cell)
        .map(str::to_string)
        .or(imported)
        .unwrap_or_else(|| sanitize_relation(cell.relation_name()))
}

/// The name "Materialize locally" suggests: the cell's relation name with a
/// `_local` suffix.
pub fn suggested_relation(cell: &Cell) -> String {
    let base = sanitize_relation(cell.relation_name());
    let mut name = format!("{base}_local");
    name.truncate(MAX_RELATION_LEN);
    name
}

/// `text` as a relation name the local database accepts: ASCII letters,
/// digits and underscores, not starting with a digit, at most 63 bytes.
pub fn sanitize_relation(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 't');
    }
    if out.starts_with("_sqlly") {
        out.insert(0, 't');
    }
    out.truncate(MAX_RELATION_LEN);
    out
}

/// Where a cell's cached output is kept between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStorage {
    /// In memory for this session only.
    #[default]
    Memory,
    /// In the encrypted snapshot store, so it survives a restart.
    EncryptedDisk,
}

impl CacheStorage {
    pub const ALL: [Self; 2] = [Self::Memory, Self::EncryptedDisk];
}

/// The cache settings a cell keeps beside its `execution_policy.cache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheSettings {
    pub storage: CacheStorage,
    /// Forget the cached output when its connection disconnects.
    pub clear_on_disconnect: bool,
}

pub fn cache_settings(cell: &Cell) -> CacheSettings {
    cell.spec
        .get(CACHE_STORAGE_KEY)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

pub fn set_cache_settings(cell: &mut Cell, settings: CacheSettings) -> bool {
    if cache_settings(cell) == settings {
        return false;
    }
    let spec = spec_object(cell);
    if settings == CacheSettings::default() {
        spec.remove(CACHE_STORAGE_KEY);
    } else {
        spec.insert(
            CACHE_STORAGE_KEY.into(),
            serde_json::to_value(settings).unwrap_or(Value::Null),
        );
    }
    tidy(cell);
    true
}

/// Whether cross-connection inputs are copied locally without asking.
pub fn auto_materialize(doc: &NotebookDoc) -> bool {
    doc.settings
        .extra
        .get(AUTO_MATERIALIZE_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn set_auto_materialize(doc: &mut NotebookDoc, on: bool) -> bool {
    if auto_materialize(doc) == on {
        return false;
    }
    if on {
        doc.settings
            .extra
            .insert(AUTO_MATERIALIZE_KEY.into(), Value::Bool(true));
    } else {
        doc.settings.extra.remove(AUTO_MATERIALIZE_KEY);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{ExecClass, ExecutionPolicy, SeededIds};
    use crate::notebook::{Dialects, ExpandOptions, Expansion, expand_sql};
    use crate::notebook::{FingerprintInputs, Graph, fingerprint_all};
    use sqlly_lang::sqlmodel::DatabaseType;
    use std::collections::{BTreeMap, BTreeSet};

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::empty("nb_local", "Local");
        for (id, source) in [
            ("orders", "SELECT id, total FROM sales.orders"),
            ("totals", "SELECT SUM(total) AS t FROM {{orders}}"),
        ] {
            let at = doc.add_cell(CellKind::Sql, None, &mut SeededIds(1));
            doc.cells[at].id = id.into();
            doc.cells[at].name = Some(id.into());
            doc.cells[at].source = source.into();
        }
        doc
    }

    #[test]
    fn the_command_line_spelling_becomes_the_spec_target() {
        let mut d = doc();
        d.connection_refs.push(crate::notebook::ConnectionRef {
            id: LOCAL_SLOT.into(),
            profile_id: None,
            label: "Local".into(),
            database: None,
            extra: Default::default(),
        });
        let totals = d.cell_mut("totals").unwrap();
        totals.connection_ref = Some(LOCAL_SLOT.into());
        totals.spec = serde_json::json!({"materialize_as": "t"});
        assert_eq!(canonicalize_local_targets(&mut d), 1);
        let totals = d.cell("totals").unwrap();
        assert_eq!(totals.connection_ref, None);
        assert_eq!(
            totals.spec,
            serde_json::json!({"materialize_as": "t", "target": "local"})
        );
        assert_eq!(totals.exec_class(), ExecClass::Local);
        assert!(d.connection_refs.iter().all(|r| r.id != LOCAL_SLOT));
        assert_eq!(canonicalize_local_targets(&mut d), 0, "already canonical");
    }

    #[test]
    fn local_target_round_trips_and_leaves_other_spec_keys() {
        let mut cell = Cell::new("c1", CellKind::Sql, ExecutionPolicy::default());
        cell.spec = serde_json::json!({"display": "grid"});
        assert!(!targets_local(&cell));
        assert_eq!(cell.exec_class(), ExecClass::Remote);
        assert!(set_targets_local(&mut cell, true));
        assert!(!set_targets_local(&mut cell, true));
        assert!(targets_local(&cell));
        assert_eq!(cell.exec_class(), ExecClass::Local);
        assert_eq!(cell.spec["display"], "grid");
        assert!(set_targets_local(&mut cell, false));
        assert_eq!(cell.spec, serde_json::json!({"display": "grid"}));

        let mut plain = Cell::new("c2", CellKind::Sql, ExecutionPolicy::default());
        set_targets_local(&mut plain, true);
        set_targets_local(&mut plain, false);
        assert!(plain.spec.is_null(), "no empty object left behind");

        let mut markdown = Cell::new("m", CellKind::Markdown, ExecutionPolicy::default());
        assert!(!set_targets_local(&mut markdown, true));

        let mut runner = Cell::new("r", CellKind::Sql, ExecutionPolicy::default());
        runner.connection_ref = Some(LOCAL_SLOT.into());
        assert!(targets_local(&runner), "the runner's local slot counts");
        assert!(set_targets_local(&mut runner, false));
        assert_eq!(runner.connection_ref, None);
        assert!(!targets_local(&runner));
    }

    #[test]
    fn relation_names_are_safe_and_suggested() {
        assert_eq!(sanitize_relation("Orders 2026"), "orders_2026");
        assert_eq!(sanitize_relation("9lives"), "t9lives");
        assert_eq!(sanitize_relation(""), "t");
        assert_eq!(sanitize_relation("_sqlly_provenance"), "t_sqlly_provenance");
        assert_eq!(sanitize_relation(&"x".repeat(100)).len(), MAX_RELATION_LEN);
        let mut cell = Cell::new("c_abc", CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some("orders".into());
        assert_eq!(suggested_relation(&cell), "orders_local");
        assert_eq!(local_relation(&cell), "orders");
        assert!(set_materialized_as(&mut cell, Some("orders_local")));
        assert_eq!(local_relation(&cell), "orders_local");
        assert!(set_materialized_as(&mut cell, None));
        assert!(cell.spec.is_null());
        for name in ["orders_2026", "t9lives", "orders_local"] {
            assert!(
                crate::local_analytics::validate_relation_name(name).is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn cache_settings_default_to_memory_and_round_trip() {
        let mut cell = Cell::new("c", CellKind::Sql, ExecutionPolicy::default());
        assert_eq!(cache_settings(&cell), CacheSettings::default());
        let disk = CacheSettings {
            storage: CacheStorage::EncryptedDisk,
            clear_on_disconnect: true,
        };
        assert!(set_cache_settings(&mut cell, disk));
        assert_eq!(cell.spec["cache_storage"]["storage"], "encrypted_disk");
        assert_eq!(cache_settings(&cell), disk);
        assert!(set_cache_settings(&mut cell, CacheSettings::default()));
        assert!(cell.spec.is_null());
    }

    #[test]
    fn auto_materialize_is_a_notebook_setting_that_survives_saving() {
        let mut d = doc();
        assert!(!auto_materialize(&d));
        assert!(set_auto_materialize(&mut d, true));
        let text = d.to_canonical_json();
        assert!(text.contains("\"local_auto_materialize\": true"), "{text}");
        let back = crate::notebook::load(None, &text, &mut SeededIds(2), None).unwrap();
        assert!(auto_materialize(&back.doc));
    }

    #[test]
    fn a_local_cell_reads_every_upstream_from_local_data() {
        let mut d = doc();
        let dialects = Dialects::single(DatabaseType::SqlServer);
        let none = BTreeSet::new();
        let opts = ExpandOptions {
            dialects: &dialects,
            materialized: &none,
        };
        assert!(matches!(
            expand_sql(&d, "totals", opts).unwrap(),
            Expansion::Sql(_)
        ));
        set_targets_local(d.cell_mut("totals").unwrap(), true);
        assert_eq!(
            expand_sql(&d, "totals", opts).unwrap(),
            Expansion::NeedsLocalMaterialization {
                cells: vec!["orders".into()]
            }
        );
        // A remote cell reading a local one cannot inline it either.
        set_targets_local(d.cell_mut("totals").unwrap(), false);
        set_targets_local(d.cell_mut("orders").unwrap(), true);
        assert_eq!(
            expand_sql(&d, "totals", opts).unwrap(),
            Expansion::NeedsLocalMaterialization {
                cells: vec!["orders".into()]
            }
        );
    }

    #[test]
    fn local_cells_plan_on_the_local_lane_without_a_connection_identity() {
        use crate::notebook::plan::{Lane, StepAction};
        use crate::notebook::{NotebookState, PlanContext, RunRequest, plan};
        let mut d = doc();
        set_targets_local(d.cell_mut("totals").unwrap(), true);
        let dialects = Dialects::single(DatabaseType::SqlServer);
        let graph = Graph::build(&d, &dialects);
        let fps = |connections: &BTreeMap<String, crate::notebook::ConnectionIdentity>| {
            fingerprint_all(
                &d,
                &graph,
                &FingerprintInputs {
                    dialects: &dialects,
                    connections,
                    params: &Default::default(),
                    upstream_override: &BTreeMap::new(),
                },
            )
        };
        let fingerprints = fps(&BTreeMap::new());
        let state = NotebookState::default();
        let p = plan(
            &RunRequest::All,
            &PlanContext {
                doc: &d,
                graph: &graph,
                fingerprints: &fingerprints,
                state: &state,
                params: &Default::default(),
                now_ms: 0,
            },
        );
        let step = p.step("totals").unwrap();
        assert_eq!(step.lane, Lane::Local);
        assert_eq!(step.action, StepAction::Execute);
        assert!(!step.requires_approval);
        let orders = p.steps.iter().position(|s| s.cell_id == "orders").unwrap();
        let totals = p.steps.iter().position(|s| s.cell_id == "totals").unwrap();
        assert_eq!(p.steps[totals].depends_on, vec![orders]);
        let limits = crate::notebook::plan::Limits::default();
        let mut schedule = crate::notebook::Schedule::new(&p, limits);
        assert_eq!(schedule.start_ready(), vec![orders]);
        let saved = schedule.statuses().to_vec();
        let mut resumed = crate::notebook::Schedule::resume(&p, limits, saved);
        assert!(resumed.start_ready().is_empty(), "the local cell waits");
        resumed.finish(orders, true);
        assert_eq!(resumed.start_ready(), vec![totals]);
        resumed.finish(totals, true);
        assert!(resumed.is_finished());
        let alone = plan(
            &RunRequest::Cell {
                id: "totals".into(),
                include_stale_upstream: false,
            },
            &PlanContext {
                doc: &d,
                graph: &graph,
                fingerprints: &fingerprints,
                state: &state,
                params: &Default::default(),
                now_ms: 0,
            },
        );
        assert!(
            alone.blocked.is_empty(),
            "a local cell copies its inputs itself, so it never waits for their outputs"
        );
        assert_eq!(alone.executing().count(), 1);
        let mut other = BTreeMap::new();
        other.insert(
            crate::notebook::fingerprint::DEFAULT_CONNECTION.to_string(),
            crate::notebook::ConnectionIdentity {
                profile_id: "p2".into(),
                ..Default::default()
            },
        );
        let moved = fps(&other);
        assert_ne!(moved["orders"].value, fingerprints["orders"].value);
        assert_eq!(
            moved["totals"].changes_from(&fingerprints["totals"]),
            vec![crate::notebook::fingerprint::ChangeReason::Upstream],
            "a local cell moves only through its inputs"
        );
    }

    #[test]
    fn an_attachment_is_read_from_the_table_it_imports_into() {
        let mut cell = Cell::new("targets", CellKind::Attachment, Default::default());
        cell.name = Some("targets".into());
        assert_eq!(local_relation(&cell), "targets");
        super::super::attachment::AttachmentSpec {
            relation: Some("region_targets".into()),
            ..Default::default()
        }
        .write(&mut cell);
        assert_eq!(local_relation(&cell), "region_targets");
    }
}
