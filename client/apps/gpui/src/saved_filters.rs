//! Saved named grid filters, persisted as JSON beside the other small app
//! stores (`saved-filters.json` in the platform data directory; localStorage
//! in the browser build).
//!
//! Filters are scoped to the connection (profile id, engine, server:port,
//! database) and to what produced the result set: a single-table statement —
//! including the `SELECT …` a table browse issues — scopes to that table, so
//! changed projections, ordering, or formatting still find the same sets;
//! any other single statement scopes to a hash of its SQL text plus the
//! result-set position. Each condition records its column **name**, and
//! applying a saved set re-targets conditions by name (see
//! [`crate::filter_lang::resolve_saved_model`]), so a set saved on
//! `SELECT *` lands on the right columns of `SELECT total, status`.
//!
//! One set per scope can be the default; it is applied automatically when a
//! fresh grid for that scope is built (with a visible notice on spill-backed
//! result sets, which cannot take client filters). Multi-statement batches
//! get a per-result-set scope but never auto-apply a default: which table a
//! batch's Nth result set came from is not something the SQL text pins down.
//!
//! The store model is pure and unit-tested. Native I/O is atomic, and a
//! store that exists but does not parse is reported and never overwritten.

use std::collections::BTreeMap;
// The browser store persists to localStorage; only the native store needs a path.
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::column_layout::{LayoutScope, single_table_target_qualified, sql_hash};
use crate::grid_filter_builder::FilterBuilderModel;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilterStore {
    /// Scope key → named filter sets, kept sorted by name.
    #[serde(default)]
    scopes: BTreeMap<String, Vec<SavedFilter>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    /// Unique within its scope; the dropdown keys by it.
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
    pub model: FilterBuilderModel,
    /// The filter-language text the model was parsed from, when it was
    /// saved from the expression field. Re-parsed at apply time so relative
    /// dates (`LAST WEEK`, `THIS MONTH`) stay relative; `model` is the
    /// fallback when the text no longer parses (a column was renamed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Column names of the result the set was saved against, indexed like
    /// the model's `ColumnTarget::Index` values. Applying re-targets each
    /// condition by this name. Empty for sets saved before names were
    /// recorded (those stay positional).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
}

/// Where a result set's saved filters live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterScope {
    /// Store key.
    pub key: String,
    /// Whether a default set may auto-apply here. False for multi-statement
    /// batches, whose result sets cannot be attributed to one table.
    pub allows_default: bool,
}

/// The saved-filter scope for result set `slot` of `sql`, run on the
/// connection `scope` describes. Unit separators keep the parts
/// unambiguous. See the module docs for the rules.
pub fn filter_scope(scope: &LayoutScope, sql: &str, slot: usize) -> FilterScope {
    let connection = format!(
        "{}\u{1f}{:?}\u{1f}{}:{}",
        scope.connection_id,
        scope.db_type,
        scope.server.trim().to_lowercase(),
        scope.port
    );
    let database = scope.database.trim().to_lowercase();
    let statements = sqlly_lang::sqllex::split_statements(sql, scope.db_type.family())
        .into_iter()
        .filter(|stmt| !stmt.text.trim().is_empty())
        .count();
    if statements > 1 {
        return FilterScope {
            key: format!(
                "batch\u{1f}{connection}\u{1f}{database}\u{1f}{}\u{1f}{slot}",
                sql_hash(sql)
            ),
            allows_default: false,
        };
    }
    if let Some((db, schema, table)) = single_table_target_qualified(sql) {
        // A three-part name names its own database.
        let database = db.map(|d| d.to_lowercase()).unwrap_or(database);
        let schema = schema
            .unwrap_or_else(|| scope.default_schema.clone())
            .to_lowercase();
        return FilterScope {
            key: format!(
                "table\u{1f}{connection}\u{1f}{database}\u{1f}{schema}\u{1f}{}",
                table.to_lowercase()
            ),
            allows_default: true,
        };
    }
    FilterScope {
        key: format!(
            "query\u{1f}{connection}\u{1f}{database}\u{1f}{}\u{1f}{slot}",
            sql_hash(sql)
        ),
        allows_default: true,
    }
}

impl SavedFilterStore {
    /// Load the store. `Err` means the file exists but could not be read or
    /// parsed: callers must show the message and must not save over it.
    pub fn load() -> Result<Self, String> {
        #[cfg(target_arch = "wasm32")]
        return Ok(crate::preferences::wasm_load_json("saved-filters.json"));
        #[cfg(not(target_arch = "wasm32"))]
        Self::load_from(&saved_filters_path())
    }

    /// Persist the store (atomically on native).
    pub fn save(&self) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::preferences::wasm_save_json("saved-filters.json", self);
            Ok(())
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.save_to(&saved_filters_path())
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_from(path: &Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store::<Self>(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => Err(crate::localization::format(
                "results.saved_filters.load_failed",
                &[("error", &crate::localization::message(error))],
            )),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save_to(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|err| {
            crate::localization::format(
                "results.saved_filters.encode_failed",
                &[("error", &err.to_string())],
            )
        })?;
        crate::fs_safe::write_atomic(path, &data).map_err(|err| {
            crate::localization::format("results.saved_filters.save_failed", &[("error", &err)])
        })
    }

    /// Saved sets for `scope`, sorted by name.
    pub fn filters_for(&self, scope: &str) -> &[SavedFilter] {
        self.scopes.get(scope).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn get(&self, scope: &str, name: &str) -> Option<&SavedFilter> {
        self.filters_for(scope)
            .iter()
            .find(|filter| filter.name == name)
    }

    /// Insert or replace the set with the same name. Replacing keeps the
    /// existing default flag unless `filter` explicitly carries one.
    pub fn upsert(&mut self, scope: &str, filter: SavedFilter) {
        let entries = self.scopes.entry(scope.to_string()).or_default();
        if let Some(existing) = entries.iter_mut().find(|f| f.name == filter.name) {
            let is_default = existing.is_default || filter.is_default;
            *existing = filter;
            existing.is_default = is_default;
        } else {
            entries.push(filter);
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Rename a set; `false` when `old` is missing or `new` is taken/empty.
    pub fn rename(&mut self, scope: &str, old: &str, new: &str) -> bool {
        let new = new.trim();
        if new.is_empty() || old == new || self.get(scope, new).is_some() {
            return false;
        }
        let Some(entries) = self.scopes.get_mut(scope) else {
            return false;
        };
        let Some(existing) = entries.iter_mut().find(|f| f.name == old) else {
            return false;
        };
        existing.name = new.to_string();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        true
    }

    pub fn delete(&mut self, scope: &str, name: &str) -> bool {
        let Some(entries) = self.scopes.get_mut(scope) else {
            return false;
        };
        let before = entries.len();
        entries.retain(|filter| filter.name != name);
        let removed = entries.len() != before;
        let now_empty = entries.is_empty();
        if now_empty {
            self.scopes.remove(scope);
        }
        removed
    }

    /// Mark `name` as the scope's default (clearing any other); `false` when
    /// the name is unknown. `None` clears the default instead.
    pub fn set_default(&mut self, scope: &str, name: Option<&str>) -> bool {
        let Some(entries) = self.scopes.get_mut(scope) else {
            return false;
        };
        if let Some(name) = name
            && !entries.iter().any(|f| f.name == name)
        {
            return false;
        }
        for filter in entries.iter_mut() {
            filter.is_default = Some(filter.name.as_str()) == name;
        }
        true
    }

    pub fn default_for(&self, scope: &str) -> Option<&SavedFilter> {
        self.filters_for(scope)
            .iter()
            .find(|filter| filter.is_default)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn saved_filters_path() -> PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push("saved-filters.json");
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    use sqlly_lang::sqlmodel::DatabaseType;

    fn model_with(a: &str) -> FilterBuilderModel {
        let mut model = FilterBuilderModel::default();
        let mut row = FilterRow::new(0, BuilderOp::Contains);
        row.a = a.to_string();
        model.push_row(row);
        model
    }

    fn saved(name: &str, a: &str) -> SavedFilter {
        SavedFilter {
            name: name.into(),
            is_default: false,
            model: model_with(a),
            expression: None,
            columns: vec!["c".into()],
        }
    }

    fn conn_scope(server: &str, database: &str) -> LayoutScope {
        LayoutScope {
            server: server.into(),
            database: database.into(),
            default_schema: "dbo".into(),
            connection_id: "conn-1".into(),
            port: 1433,
            db_type: DatabaseType::SqlServer,
        }
    }

    fn key(scope: &LayoutScope, sql: &str) -> String {
        filter_scope(scope, sql, 0).key
    }

    #[test]
    fn table_scope_survives_projection_order_and_whitespace() {
        let scope = conn_scope("host-a", "shop");
        let browse = key(&scope, "SELECT * FROM dbo.orders");
        let reformatted = key(&scope, "select  *\n  from dbo.orders");
        let projection = key(
            &scope,
            "SELECT id, total, ordered_at FROM dbo.orders ORDER BY ordered_at DESC",
        );
        let filtered = key(&scope, "SELECT * FROM dbo.orders WHERE total > 10");
        assert_eq!(browse, reformatted, "whitespace does not split a scope");
        assert_eq!(browse, projection, "same table, different projection");
        assert_eq!(browse, filtered, "same table, different predicate");
        assert!(filter_scope(&scope, "SELECT * FROM dbo.orders", 0).allows_default);
    }

    #[test]
    fn scope_separates_connections_ports_engines_and_tables() {
        let base = conn_scope("host-a", "shop");
        let table = key(&base, "SELECT * FROM dbo.orders");
        let other_server = key(&conn_scope("host-b", "shop"), "SELECT * FROM dbo.orders");
        let other_database = key(
            &conn_scope("host-a", "warehouse"),
            "SELECT * FROM dbo.orders",
        );
        let other_table = key(&base, "SELECT * FROM dbo.customers");
        let other_port = key(
            &LayoutScope {
                port: 1434,
                ..base.clone()
            },
            "SELECT * FROM dbo.orders",
        );
        let other_connection = key(
            &LayoutScope {
                connection_id: "conn-2".into(),
                ..base.clone()
            },
            "SELECT * FROM dbo.orders",
        );
        let other_engine = key(
            &LayoutScope {
                db_type: DatabaseType::Postgres,
                ..base.clone()
            },
            "SELECT * FROM dbo.orders",
        );
        for (other, why) in [
            (other_server, "another server"),
            (other_database, "another database"),
            (other_table, "another table"),
            (
                other_port,
                "another port (a second instance on the same host)",
            ),
            (other_connection, "another connection profile"),
            (other_engine, "another engine"),
        ] {
            assert_ne!(table, other, "{why}");
        }
    }

    #[test]
    fn three_part_names_scope_to_their_own_database() {
        let scope = conn_scope("host-a", "shop");
        let qualified = key(&scope, "SELECT * FROM warehouse.dbo.orders");
        assert_ne!(
            qualified,
            key(&scope, "SELECT * FROM dbo.orders"),
            "warehouse.dbo.orders is not shop.dbo.orders"
        );
        assert_eq!(
            qualified,
            key(
                &conn_scope("host-a", "warehouse"),
                "SELECT * FROM dbo.orders"
            ),
            "same table reached from its own database"
        );
    }

    #[test]
    fn non_table_sql_scopes_to_a_sql_hash_and_result_position() {
        let scope = conn_scope("host-a", "shop");
        let a = key(&scope, "SELECT  *\nFROM a JOIN b ON a.x = b.x");
        let b = key(&scope, "SELECT * FROM a JOIN b ON a.x = b.x");
        assert_eq!(a, b, "whitespace does not split a query scope");
        let edited = key(&scope, "SELECT * FROM a JOIN b ON a.y = b.y");
        assert_ne!(a, edited, "edited SQL is a different scope");
        let other_server = key(
            &conn_scope("host-b", "shop"),
            "SELECT * FROM a JOIN b ON a.x = b.x",
        );
        assert_ne!(a, other_server, "queries do not cross connections");
        let proc_first = filter_scope(&scope, "EXEC dbo.report", 0);
        let proc_second = filter_scope(&scope, "EXEC dbo.report", 1);
        assert_ne!(proc_first.key, proc_second.key, "each result set of a proc");
        assert!(proc_first.allows_default);
    }

    #[test]
    fn batches_scope_per_result_set_and_never_auto_apply() {
        let scope = conn_scope("host-a", "shop");
        let sql = "SELECT * FROM dbo.orders; SELECT * FROM dbo.customers";
        let first = filter_scope(&scope, sql, 0);
        let second = filter_scope(&scope, sql, 1);
        assert_ne!(first.key, second.key, "result sets do not share a scope");
        assert!(!first.allows_default && !second.allows_default);
        assert_ne!(
            first.key,
            key(&scope, "SELECT * FROM dbo.orders"),
            "a batch never borrows a table's default"
        );
    }

    #[test]
    fn upsert_inserts_and_replaces_by_name() {
        let mut store = SavedFilterStore::default();
        let scope = "s";
        store.upsert(scope, saved("b filter", "x"));
        store.upsert(scope, saved("a filter", "y"));
        let names: Vec<&str> = store
            .filters_for(scope)
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["a filter", "b filter"], "kept sorted by name");
        let mut replacement = saved("a filter", "z");
        store.upsert(scope, replacement.clone());
        assert_eq!(store.filters_for(scope).len(), 2);
        assert_eq!(store.get(scope, "a filter"), Some(&replacement));
        // Replacing a default keeps it the default.
        replacement.is_default = true;
        store.upsert(scope, replacement);
        let mut not_default = saved("a filter", "w");
        store.upsert(scope, not_default.clone());
        not_default.is_default = true;
        assert_eq!(store.get(scope, "a filter"), Some(&not_default));
    }

    #[test]
    fn rename_rules() {
        let mut store = SavedFilterStore::default();
        let scope = "s";
        store.upsert(scope, saved("one", "x"));
        store.upsert(scope, saved("two", "y"));
        assert!(store.rename(scope, "one", "three"));
        assert!(store.get(scope, "three").is_some());
        assert!(!store.rename(scope, "missing", "x"));
        assert!(!store.rename(scope, "three", "two"), "target name taken");
        assert!(!store.rename(scope, "three", "  "), "blank name rejected");
        assert!(!store.rename("no scope", "three", "x"));
    }

    #[test]
    fn delete_removes_scope_when_empty() {
        let mut store = SavedFilterStore::default();
        let scope = "s";
        store.upsert(scope, saved("only", "x"));
        assert!(store.delete(scope, "only"));
        assert!(store.filters_for(scope).is_empty());
        assert!(!store.delete(scope, "only"));
        assert!(!store.delete("no scope", "x"));
    }

    #[test]
    fn default_is_exclusive_per_scope() {
        let mut store = SavedFilterStore::default();
        let scope = "s";
        store.upsert(scope, saved("one", "x"));
        store.upsert(scope, saved("two", "y"));
        assert!(store.set_default(scope, Some("one")));
        assert_eq!(
            store.default_for(scope).map(|f| f.name.as_str()),
            Some("one")
        );
        assert!(store.set_default(scope, Some("two")));
        assert_eq!(
            store.default_for(scope).map(|f| f.name.as_str()),
            Some("two")
        );
        assert!(!store.get(scope, "one").expect("one").is_default);
        assert!(store.set_default(scope, None));
        assert!(store.default_for(scope).is_none());
        assert!(!store.set_default(scope, Some("missing")));
        assert!(!store.set_default("no scope", Some("one")));
        // Defaults are per-scope: another scope's default is independent.
        store.upsert("other", saved("o", "z"));
        assert!(store.set_default("other", Some("o")));
        assert!(store.set_default(scope, Some("two")));
        assert_eq!(
            store.default_for("other").map(|f| f.name.as_str()),
            Some("o")
        );
        assert_eq!(
            store.default_for(scope).map(|f| f.name.as_str()),
            Some("two")
        );
    }

    #[test]
    fn serde_round_trip_and_legacy_shapes() {
        let mut store = SavedFilterStore::default();
        let scope = key(&conn_scope("host-a", "shop"), "SELECT * FROM t");
        let mut default_filter = saved("mine", "abc");
        default_filter.is_default = true;
        store.upsert(&scope, default_filter);
        let json = serde_json::to_string_pretty(&store).unwrap();
        let back: SavedFilterStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);
        // A store written before scopes existed decodes empty, not an error.
        let empty: SavedFilterStore = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, SavedFilterStore::default());
        // A set written before column names were recorded still loads.
        let legacy =
            r#"{"scopes":{"s":[{"name":"n","model":{"root":{"conjunction":"And","items":[]}}}]}}"#;
        let legacy: SavedFilterStore = serde_json::from_str(legacy).unwrap();
        assert!(legacy.get("s", "n").unwrap().columns.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn corrupt_store_is_reported_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("saved-filters.json");
        assert_eq!(
            SavedFilterStore::load_from(&path),
            Ok(SavedFilterStore::default()),
            "a missing file is an empty store"
        );
        std::fs::write(&path, b"{not json").unwrap();
        let err = SavedFilterStore::load_from(&path).unwrap_err();
        assert!(err.contains("could not be loaded"), "{err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{not json",
            "loading leaves the user's file alone"
        );
        // Saving writes atomically and round-trips.
        let mut store = SavedFilterStore::default();
        store.upsert("s", saved("one", "x"));
        store.save_to(&path).unwrap();
        assert_eq!(SavedFilterStore::load_from(&path), Ok(store));
    }
}
