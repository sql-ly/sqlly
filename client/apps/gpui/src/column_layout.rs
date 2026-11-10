//! Persisted result-grid column layouts keyed by table or normalized query text.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlly_core::{ConnectionProfile, ResultColumn};
use sqlly_datatable::GridState;
use sqlly_intel::query::tokenizer::{TokenKind, tokenize};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::grid_sort::SortSpec;
use crate::host_storage::StorageArea;

const LAYOUT_VERSION: u32 = 1;
pub(crate) const MIN_COLUMN_WIDTH: f32 = 40.0;
pub(crate) const MAX_COLUMN_WIDTH: f32 = 4000.0;

/// Connection scope for layout keys (server + database + default schema)
/// and saved-filter scopes, which additionally key on the connection
/// profile, port, and engine (see [`crate::saved_filters::filter_scope`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutScope {
    pub server: String,
    pub database: String,
    pub default_schema: String,
    /// Connection profile id. Saved-filter scopes only.
    pub connection_id: String,
    /// Server port: two instances on one host are different servers.
    /// Saved-filter scopes only.
    pub port: u16,
    /// Engine, which also picks the statement splitter for batch detection.
    /// Saved-filter scopes only.
    pub db_type: DatabaseType,
}

impl LayoutScope {
    pub fn from_profile(profile: &ConnectionProfile) -> Self {
        Self {
            server: profile.server.trim().to_lowercase(),
            database: profile.database.clone().unwrap_or_default().to_lowercase(),
            default_schema: default_schema_for(profile.database_type),
            connection_id: profile.id.0.to_string(),
            port: profile.port,
            db_type: profile.database_type,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ColumnLayoutKey {
    Table {
        server: String,
        database: String,
        schema: String,
        table: String,
    },
    Query {
        server: String,
        database: String,
        sql_hash: String,
    },
}

/// Names + declared types in result order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnSignature(pub Vec<(String, String)>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnLayout {
    pub version: u32,
    pub signature: ColumnSignature,
    pub widths: Vec<f32>,
    pub order: Vec<usize>,
    pub hidden: Vec<usize>,
    pub frozen: usize,
    #[serde(default)]
    pub sort: SortSpec,
    /// Per-column FK label choices: `(local column, display column or None = off)`.
    #[serde(default)]
    pub fk_labels: Vec<(String, Option<String>)>,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnLayoutStore {
    #[serde(default)]
    pub entries: Vec<(ColumnLayoutKey, ColumnLayout)>,
}

impl ColumnLayoutStore {
    pub const CAP: usize = 256;
    pub const FILE: &'static str = "column-layouts.json";

    pub fn get(&self, key: &ColumnLayoutKey) -> Option<&ColumnLayout> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, layout)| layout)
    }

    pub fn layout_for(
        &self,
        key: &ColumnLayoutKey,
        sig: &ColumnSignature,
    ) -> Option<&ColumnLayout> {
        let layout = self.get(key)?;
        if layout.version != LAYOUT_VERSION {
            return None;
        }
        if layout.signature != *sig {
            return None;
        }
        if layout.widths.len() != sig.0.len() {
            return None;
        }
        Some(layout)
    }

    pub fn upsert(&mut self, key: ColumnLayoutKey, layout: ColumnLayout) {
        self.entries.retain(|(k, _)| k != &key);
        self.entries.insert(0, (key, layout));
        self.entries.truncate(Self::CAP);
    }

    pub fn forget(&mut self, key: &ColumnLayoutKey) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| k != key);
        self.entries.len() != before
    }

    /// Load the store. A missing file is an empty store; a file that exists
    /// but cannot be read or parsed is `Err` — callers surface it and must
    /// not save over it (the user's layouts are in that file).
    pub fn load() -> Result<Self, String> {
        #[cfg(target_arch = "wasm32")]
        {
            match crate::host_storage::load_text(StorageArea::Data, Self::FILE) {
                None => Ok(Self::default()),
                Some(text) => serde_json::from_str(&text).map_err(|err| {
                    crate::localization::format(
                        "results.column_layout.browser_load_failed",
                        &[("error", &err.to_string())],
                    )
                }),
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => Ok(Self::default()),
        }
    }

    /// Persist the store (atomically on native).
    pub fn save(&self) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            let text = serde_json::to_string_pretty(self).map_err(|err| {
                crate::localization::format(
                    "results.column_layout.browser_encode_failed",
                    &[("error", &err.to_string())],
                )
            })?;
            crate::host_storage::store_text(StorageArea::Data, Self::FILE, &text).map_err(|err| {
                crate::localization::format(
                    "results.column_layout.browser_save_failed",
                    &[("error", &err)],
                )
            })
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = Self::path().ok_or_else(|| {
                crate::localization::text("results.column_layout.no_data_directory")
            })?;
            self.save_to(&path)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn path() -> Option<std::path::PathBuf> {
        crate::host_storage::artifact_path(StorageArea::Data, Self::FILE)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn load_from(path: &std::path::Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store::<Self>(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => Err(crate::localization::format(
                "results.column_layout.native_load_failed",
                &[("error", &crate::localization::message(error))],
            )),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|err| {
            crate::localization::format(
                "results.column_layout.native_encode_failed",
                &[("error", &err.to_string())],
            )
        })?;
        crate::fs_safe::write_atomic(path, &data).map_err(|err| {
            crate::localization::format(
                "results.column_layout.native_save_failed",
                &[("error", &err)],
            )
        })
    }
}

pub fn normalized_sql(sql: &str) -> String {
    let tokens = tokenize(sql);
    let mut out = String::new();
    let mut pending_space = false;

    for token in tokens {
        match token.kind {
            TokenKind::Comment | TokenKind::Whitespace => {
                if !out.is_empty() {
                    pending_space = true;
                }
            }
            TokenKind::Keyword | TokenKind::Identifier | TokenKind::Symbol => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push_str(&token.text.to_ascii_lowercase());
            }
            TokenKind::StringLiteral | TokenKind::Number => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push_str(token.text);
            }
        }
    }

    out.trim().to_string()
}

pub fn sql_hash(sql: &str) -> String {
    let normalized = normalized_sql(sql);
    let digest = Sha256::digest(normalized.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>()
}

pub fn single_table_target(sql: &str) -> Option<(Option<String>, String)> {
    single_table_target_qualified(sql).map(|(_, schema, table)| (schema, table))
}

/// [`single_table_target`] keeping a three-part name's database:
/// `(database, schema, table)`. Column layouts key on schema + table only;
/// saved filters also honour the database part.
pub fn single_table_target_qualified(
    sql: &str,
) -> Option<(Option<String>, Option<String>, String)> {
    let tokens: Vec<_> = tokenize(sql)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect();

    if tokens.is_empty() {
        return None;
    }

    if !is_keyword(&tokens[0], "select") {
        return None;
    }

    let from_positions: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == TokenKind::Keyword && eq_ascii_ignore_case(t.text, "from"))
        .map(|(i, _)| i)
        .collect();
    if from_positions.len() != 1 {
        return None;
    }
    let from_idx = from_positions[0];

    for token in tokens.iter().skip(from_idx + 1) {
        if token.kind == TokenKind::Keyword && eq_ascii_ignore_case(token.text, "join") {
            return None;
        }
        if token.kind == TokenKind::Symbol && token.text == "(" {
            return None;
        }
    }

    let mut idx = from_idx + 1;
    let mut parts: Vec<String> = Vec::new();
    while idx < tokens.len() {
        let token = &tokens[idx];
        if token.kind == TokenKind::Identifier || token.kind == TokenKind::Keyword {
            parts.push(normalize_ident(token.text));
            idx += 1;
            if idx < tokens.len()
                && tokens[idx].kind == TokenKind::Symbol
                && tokens[idx].text == "."
            {
                idx += 1;
                continue;
            }
            break;
        }
        return None;
    }

    if parts.is_empty() || parts.len() > 3 {
        return None;
    }

    let (database, schema, table) = match parts.len() {
        1 => (None, None, parts[0].clone()),
        2 => (None, Some(parts[0].clone()), parts[1].clone()),
        3 => (
            Some(parts[0].clone()),
            Some(parts[1].clone()),
            parts[2].clone(),
        ),
        _ => return None,
    };

    if !table_post_from_tail_valid(&tokens, idx) {
        return None;
    }

    Some((database, schema, table))
}

fn table_post_from_tail_valid(
    tokens: &[sqlly_intel::query::tokenizer::Token<'_>],
    start: usize,
) -> bool {
    if start >= tokens.len() {
        return true;
    }
    let token = &tokens[start];
    if token.kind == TokenKind::Symbol && token.text == ";" {
        return start + 1 >= tokens.len();
    }
    if token.kind == TokenKind::Keyword {
        let kw = token.text.to_ascii_lowercase();
        if matches!(
            kw.as_str(),
            "where" | "order" | "group" | "having" | "with" | "option" | "limit" | "offset"
        ) {
            return true;
        }
        if kw == "as" {
            let next = tokens.get(start + 1);
            return next
                .is_some_and(|t| t.kind == TokenKind::Identifier || t.kind == TokenKind::Keyword);
        }
        return false;
    }
    if token.kind == TokenKind::Identifier {
        // Bare table alias.
        return true;
    }
    false
}

pub fn key_for(server: &str, database: &str, default_schema: &str, sql: &str) -> ColumnLayoutKey {
    let server = server.trim().to_lowercase();
    let database = database.trim().to_lowercase();
    if let Some((schema, table)) = single_table_target(sql) {
        let schema = schema
            .unwrap_or_else(|| default_schema.to_string())
            .to_lowercase();
        ColumnLayoutKey::Table {
            server,
            database,
            schema,
            table: table.to_lowercase(),
        }
    } else {
        ColumnLayoutKey::Query {
            server,
            database,
            sql_hash: sql_hash(sql),
        }
    }
}

pub fn signature_for(columns: &[ResultColumn]) -> ColumnSignature {
    ColumnSignature(
        columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone()))
            .collect(),
    )
}

/// Apply a saved layout to a grid whose first `marker` columns are app
/// chrome (the staged-edit marker column), not result columns. Layouts are
/// stored in result-column indices; the marker stays leftmost and visible.
pub fn apply_to_grid(state: &mut GridState, layout: &ColumnLayout, marker: usize) {
    let marker = marker.min(state.data.columns.len());
    for (col, width) in state.data.columns[marker..].iter_mut().zip(&layout.widths) {
        col.width = width.clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH);
    }
    let result_count = state.data.columns.len() - marker;
    // Empty `order` is how pre-4.4 layouts were saved (identity). Applying it
    // would hide every column; skip and keep the grid's current display order.
    if !layout.order.is_empty() && column_order_is_usable(&layout.order, result_count) {
        let order: Vec<usize> = (0..marker)
            .chain(layout.order.iter().map(|&idx| idx + marker))
            .collect();
        state.set_column_order(&order);
    }
    // Frozen counts display columns from the left; the marker, when present,
    // is one of them.
    let frozen = if layout.frozen > 0 {
        layout.frozen + marker
    } else {
        0
    };
    state.set_frozen_columns(frozen);
}

/// A sort spec moved between result-column and grid-column indices: `delta`
/// is `+marker` going to the grid and `-marker` coming back. A key on the
/// marker column itself lands before column 0 and is dropped.
pub fn shift_sort_spec(spec: &SortSpec, delta: isize) -> SortSpec {
    SortSpec {
        keys: spec
            .keys
            .iter()
            .filter_map(|key| {
                let column = key.column.checked_add_signed(delta)?;
                Some(crate::grid_sort::SortKey {
                    column,
                    descending: key.descending,
                })
            })
            .collect(),
    }
}

/// True when `order` is a duplicate-free subset of `0..n` (omitted indices are hidden).
#[must_use]
pub fn column_order_is_usable(order: &[usize], n: usize) -> bool {
    let mut seen = vec![false; n];
    for &idx in order {
        if idx >= n || seen[idx] {
            return false;
        }
        seen[idx] = true;
    }
    true
}

/// Capture a grid's layout in result-column indices. `marker` leading grid
/// columns (the staged-edit marker) are not result columns and are left
/// out; `sort` is in grid columns like the rest of the grid state.
pub fn capture_from_grid(
    state: &GridState,
    sig: &ColumnSignature,
    sort: &SortSpec,
    marker: usize,
) -> ColumnLayout {
    let marker = marker.min(state.data.columns.len());
    let to_result = |idx: &usize| idx.checked_sub(marker);
    ColumnLayout {
        version: LAYOUT_VERSION,
        signature: sig.clone(),
        widths: state.data.columns[marker..]
            .iter()
            .map(|c| c.width)
            .collect(),
        order: state.column_order().iter().filter_map(to_result).collect(),
        hidden: state
            .hidden_columns()
            .iter()
            .filter_map(to_result)
            .collect(),
        frozen: state.frozen_columns().saturating_sub(marker),
        sort: shift_sort_spec(sort, -(marker as isize)),
        fk_labels: Vec::new(),
    }
}

fn default_schema_for(db: DatabaseType) -> String {
    match db {
        DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => "dbo".to_string(),
        DatabaseType::Postgres
        | DatabaseType::CockroachDb
        | DatabaseType::YugabyteDb
        | DatabaseType::AmazonAuroraPostgres
        | DatabaseType::AmazonRdsPostgres
        | DatabaseType::AzurePostgresFlexible
        | DatabaseType::CloudSqlPostgres
        | DatabaseType::Neon
        | DatabaseType::Supabase => "public".to_string(),
        DatabaseType::MySql
        | DatabaseType::MariaDb
        | DatabaseType::Sqlite
        | DatabaseType::AmazonAurora
        | DatabaseType::AmazonRds
        | DatabaseType::AzureMySqlFlexible
        | DatabaseType::CloudSqlMySql
        | DatabaseType::PlanetScale
        | DatabaseType::TiDb => String::new(),
        _ => String::new(),
    }
}

fn normalize_ident(text: &str) -> String {
    let trimmed = text.trim();
    let stripped = trimmed.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'));
    stripped.to_lowercase()
}

fn is_keyword(token: &sqlly_intel::query::tokenizer::Token<'_>, word: &str) -> bool {
    token.kind == TokenKind::Keyword && eq_ascii_ignore_case(token.text, word)
}

fn eq_ascii_ignore_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_key(server: &str, database: &str, schema: &str, table: &str) -> ColumnLayoutKey {
        ColumnLayoutKey::Table {
            server: server.to_string(),
            database: database.to_string(),
            schema: schema.to_string(),
            table: table.to_string(),
        }
    }

    fn sig(names: &[(&str, &str)]) -> ColumnSignature {
        ColumnSignature(
            names
                .iter()
                .map(|(n, t)| (n.to_string(), t.to_string()))
                .collect(),
        )
    }

    fn sample_layout(sig: &ColumnSignature, widths: &[f32]) -> ColumnLayout {
        let n = sig.0.len();
        ColumnLayout {
            version: LAYOUT_VERSION,
            signature: sig.clone(),
            widths: widths.to_vec(),
            order: (0..n).collect(),
            hidden: vec![],
            frozen: 0,
            sort: SortSpec::default(),
            fk_labels: Vec::new(),
        }
    }

    #[test]
    fn normalized_sql_collapses_whitespace_and_case() {
        let a = normalized_sql("SELECT  a,\n\tB FROM  T");
        let b = normalized_sql("select a, b from t");
        assert_eq!(a, b);
    }

    #[test]
    fn normalized_sql_drops_comments() {
        let a = normalized_sql("SELECT a -- note\nFROM t /* x */");
        let b = normalized_sql("select a from t");
        assert_eq!(a, b);
    }

    #[test]
    fn normalized_sql_keeps_literals() {
        let upper = normalized_sql("SELECT 'Abc'");
        let lower = normalized_sql("SELECT 'abc'");
        assert_ne!(upper, lower);
        assert!(normalized_sql("WHERE x = 10").contains("10"));
    }

    #[test]
    fn sql_hash_is_stable_and_hex() {
        let sql = "select a from t";
        let h1 = sql_hash(sql);
        let h2 = sql_hash(sql);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(
            h1.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(sql_hash("select a from t where x = 1"), h1);
    }

    #[test]
    fn key_for_simple_select_is_table_key() {
        let key = key_for(
            "MyHost",
            "db",
            "dbo",
            "SELECT TOP 1000 * FROM [Sales].[Orders] WHERE id > 1",
        );
        assert_eq!(key, table_key("myhost", "db", "sales", "orders"));

        let key = key_for("h", "d", "dbo", "select * from orders o order by 1");
        assert_eq!(key, table_key("h", "d", "dbo", "orders"));
    }

    #[test]
    fn key_for_join_or_cte_is_query_key() {
        let server = "h";
        let database = "d";
        let cases = [
            "SELECT * FROM a JOIN b ON a.id = b.id",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "SELECT * FROM a, b",
            "SELECT * FROM (SELECT 1) x",
            "SELECT 1; SELECT 2",
        ];
        for sql in cases {
            let key = key_for(server, database, "dbo", sql);
            match key {
                ColumnLayoutKey::Query { sql_hash, .. } => {
                    assert!(!sql_hash.is_empty());
                }
                ColumnLayoutKey::Table { .. } => panic!("expected query key for {sql}"),
            }
        }
    }

    #[test]
    fn three_part_name_drops_database_qualifier() {
        let key = key_for("h", "d", "dbo", "SELECT * FROM db1.dbo.T");
        assert_eq!(key, table_key("h", "d", "dbo", "t"));
    }

    #[test]
    fn qualified_target_keeps_the_database_part() {
        assert_eq!(
            single_table_target_qualified("SELECT * FROM db1.dbo.T"),
            Some((Some("db1".into()), Some("dbo".into()), "t".into()))
        );
        assert_eq!(
            single_table_target_qualified("SELECT * FROM dbo.T"),
            Some((None, Some("dbo".into()), "t".into()))
        );
        assert_eq!(
            single_table_target_qualified("SELECT * FROM T"),
            Some((None, None, "t".into()))
        );
    }

    #[test]
    fn signature_uses_name_and_type_in_order() {
        let s1 = sig(&[("a", "int"), ("b", "text")]);
        let s2 = sig(&[("b", "text"), ("a", "int")]);
        let s3 = sig(&[("a", "bigint"), ("b", "text")]);
        assert_ne!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn layout_for_ignores_signature_mismatch() {
        let key = table_key("h", "d", "dbo", "t");
        let s1 = sig(&[("id", "int")]);
        let s2 = sig(&[("id", "bigint")]);
        let mut store = ColumnLayoutStore::default();
        store.upsert(key.clone(), sample_layout(&s1, &[100.0]));
        assert!(store.layout_for(&key, &s2).is_none());
        assert!(store.layout_for(&key, &s1).is_some());
    }

    #[test]
    fn layout_for_rejects_width_count_mismatch_and_wrong_version() {
        let key = table_key("h", "d", "dbo", "t");
        let sig = sig(&[("id", "int"), ("name", "text")]);
        let mut store = ColumnLayoutStore::default();
        store.upsert(key.clone(), sample_layout(&sig, &[100.0]));
        let bad_widths = ColumnLayout {
            version: LAYOUT_VERSION,
            signature: sig.clone(),
            widths: vec![100.0],
            order: vec![0, 1],
            hidden: vec![],
            frozen: 0,
            sort: SortSpec::default(),
            fk_labels: vec![],
        };
        store.upsert(key.clone(), bad_widths);
        assert!(store.layout_for(&key, &sig).is_none());

        let bad_version = ColumnLayout {
            version: 0,
            signature: sig.clone(),
            widths: vec![100.0, 200.0],
            order: vec![0, 1],
            hidden: vec![],
            frozen: 0,
            sort: SortSpec::default(),
            fk_labels: vec![],
        };
        store.upsert(key.clone(), bad_version);
        assert!(store.layout_for(&key, &sig).is_none());
    }

    #[test]
    fn upsert_moves_to_front_and_evicts_past_cap() {
        let mut store = ColumnLayoutStore::default();
        let first_key = table_key("h", "d", "dbo", "first");
        let sig = sig(&[("id", "int")]);
        store.upsert(first_key.clone(), sample_layout(&sig, &[1.0]));
        for i in 0..256 {
            let key = table_key("h", "d", "dbo", &format!("t{i}"));
            store.upsert(key, sample_layout(&sig, &[2.0]));
        }
        assert_eq!(store.entries.len(), 256);
        assert!(store.get(&first_key).is_none());

        let repeat = table_key("h", "d", "dbo", "t255");
        store.upsert(repeat.clone(), sample_layout(&sig, &[3.0]));
        assert_eq!(store.entries.len(), 256);
        assert_eq!(store.entries[0].0, repeat);
    }

    #[test]
    fn store_json_round_trip() {
        let mut store = ColumnLayoutStore::default();
        let key = table_key("h", "d", "dbo", "orders");
        let sig = sig(&[("id", "int")]);
        store.upsert(key, sample_layout(&sig, &[150.0]));
        let text = serde_json::to_string(&store).expect("serialize");
        let decoded: ColumnLayoutStore = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(decoded, store);

        let empty: ColumnLayoutStore = serde_json::from_str("{}").expect("empty default");
        assert!(empty.entries.is_empty());
    }

    #[test]
    fn layout_json_round_trips_sort_and_old_layout_defaults_empty() {
        let mut layout = sample_layout(&sig(&[("id", "int"), ("name", "text")]), &[80.0, 120.0]);
        layout.sort = SortSpec {
            keys: vec![
                crate::grid_sort::SortKey {
                    column: 1,
                    descending: false,
                },
                crate::grid_sort::SortKey {
                    column: 0,
                    descending: true,
                },
            ],
        };
        let text = serde_json::to_string(&layout).expect("serialize");
        let decoded: ColumnLayout = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(decoded.sort, layout.sort);

        let old = r#"{"version":1,"signature":[["id","int"]],"widths":[80.0],"order":[0],"hidden":[],"frozen":0}"#;
        let decoded_old: ColumnLayout = serde_json::from_str(old).expect("old layout");
        assert!(decoded_old.sort.keys.is_empty());
        assert_eq!(decoded_old.version, LAYOUT_VERSION);
    }

    #[test]
    fn apply_ignores_invalid_order() {
        assert!(!column_order_is_usable(&[0, 99], 2));
        assert!(!column_order_is_usable(&[0, 0], 2));
        assert!(column_order_is_usable(&[1, 0], 2));
        assert!(column_order_is_usable(&[0], 2));
        assert!(column_order_is_usable(&[], 2));
    }

    #[test]
    fn layout_round_trips_fk_labels() {
        let sig = sig(&[("customer_id", "int")]);
        let layout = ColumnLayout {
            version: LAYOUT_VERSION,
            signature: sig.clone(),
            widths: vec![120.0],
            order: vec![0],
            hidden: vec![],
            frozen: 0,
            sort: SortSpec::default(),
            fk_labels: vec![("customer_id".into(), Some("name".into()))],
        };
        let json = serde_json::to_string(&layout).expect("serialize");
        let back: ColumnLayout = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.fk_labels, layout.fk_labels);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn corrupt_store_is_reported_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ColumnLayoutStore::FILE);
        assert_eq!(
            ColumnLayoutStore::load_from(&path),
            Ok(ColumnLayoutStore::default()),
            "a missing file is an empty store"
        );
        std::fs::write(&path, b"{\"entries\": [oops").unwrap();
        let err = ColumnLayoutStore::load_from(&path).unwrap_err();
        assert!(err.contains("could not be loaded"), "{err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"entries\": [oops",
            "loading leaves the user's file alone"
        );
        let mut store = ColumnLayoutStore::default();
        let sig = sig(&[("id", "int")]);
        store.upsert(
            table_key("h", "d", "dbo", "t"),
            sample_layout(&sig, &[90.0]),
        );
        store.save_to(&path).unwrap();
        assert_eq!(ColumnLayoutStore::load_from(&path), Ok(store));
    }

    #[test]
    fn legacy_layout_without_fk_labels_deserialises() {
        let old = r#"{"version":1,"signature":[["id","int"]],"widths":[80.0],"order":[0],"hidden":[],"frozen":0}"#;
        let layout: ColumnLayout = serde_json::from_str(old).expect("deserialize");
        assert!(layout.fk_labels.is_empty());
    }
}
