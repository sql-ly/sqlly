//! Cell fingerprints (spec 5.2): the cache key of a cell's output. A cell's
//! fingerprint covers its normalized source, kind and spec, the connection
//! identity it runs as (profile, database, login, engine, policy flags,
//! session settings), its execution policy, the typed values of the
//! parameters it uses, and its upstream cells' fingerprints. It changes when
//! any of those change and is stable across runs and machines otherwise.
//!
//! A fingerprint never proves the source data is unchanged; freshness of
//! the data itself is a separate label on the output.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlly_lang::sqllex::{StrictLexer, TokenKind};
use sqlly_lang::sqlmodel::DatabaseType;

use super::document::{Cell, NotebookDoc, Sorted, SourceLanguage};
use super::graph::{Graph, Node};
use super::params::ResolvedParams;
use super::references::Dialects;

const DOMAIN: &[u8] = b"sqlly.notebook.fingerprint.v1";

/// A SHA-256 digest in lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(pub String);

impl Fingerprint {
    /// The first 12 hex digits, for display.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }
}

/// Field-separated hashing so `("ab","c")` and `("a","bc")` differ.
struct Hasher(Sha256);

impl Hasher {
    fn new(tag: &str) -> Self {
        let mut h = Hasher(Sha256::new());
        h.field(DOMAIN);
        h.field(tag.as_bytes());
        h
    }

    fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    fn text(&mut self, text: &str) -> &mut Self {
        self.field(text.as_bytes())
    }

    fn finish(self) -> Fingerprint {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let digest = self.0.finalize();
        let mut out = String::with_capacity(digest.len() * 2);
        for b in digest {
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 0xf)] as char);
        }
        Fingerprint(out)
    }
}

/// Who a cell's query runs as, supplied by the host. Two cells with
/// different identities never share a cached output.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConnectionIdentity {
    /// The opaque connection profile id.
    pub profile_id: String,
    pub database: Option<String>,
    /// The effective login (user, role, tenant), as the host knows it.
    pub login_identity: String,
    /// Engine and server version, e.g. `SQL Server 16.0.4135`.
    pub engine: String,
    /// Safety flags in force (`read_only`, `block_all_but_select`, …).
    pub policy_flags: BTreeSet<String>,
    /// Result-affecting session settings (time zone, collation, language).
    pub session_settings: BTreeMap<String, String>,
}

/// Key under which the host's current connection is supplied when a cell
/// has no connection slot.
pub const DEFAULT_CONNECTION: &str = "";

/// The components of a fingerprint, each hashed on its own, so a change can
/// be explained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintParts {
    pub source: Fingerprint,
    pub kind_spec: Fingerprint,
    pub connection: Fingerprint,
    pub policy: Fingerprint,
    pub params: Fingerprint,
    pub upstream: Fingerprint,
}

/// What differs between two fingerprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeReason {
    Source,
    KindOrSpec,
    Connection,
    Policy,
    Parameters,
    Upstream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellFingerprint {
    pub value: Fingerprint,
    pub parts: FingerprintParts,
}

impl CellFingerprint {
    /// Which components changed since `older`, in a fixed order.
    pub fn changes_from(&self, older: &CellFingerprint) -> Vec<ChangeReason> {
        let (a, b) = (&self.parts, &older.parts);
        [
            (a.source != b.source, ChangeReason::Source),
            (a.kind_spec != b.kind_spec, ChangeReason::KindOrSpec),
            (a.connection != b.connection, ChangeReason::Connection),
            (a.policy != b.policy, ChangeReason::Policy),
            (a.params != b.params, ChangeReason::Parameters),
            (a.upstream != b.upstream, ChangeReason::Upstream),
        ]
        .into_iter()
        .filter_map(|(changed, reason)| changed.then_some(reason))
        .collect()
    }
}

/// Everything outside the document a fingerprint depends on.
#[derive(Debug, Clone, Copy)]
pub struct FingerprintInputs<'a> {
    pub dialects: &'a Dialects,
    /// By connection slot id ([`DEFAULT_CONNECTION`] for the host's
    /// current connection). A missing slot hashes as "unmapped".
    pub connections: &'a BTreeMap<String, ConnectionIdentity>,
    pub params: &'a ResolvedParams,
    /// Output fingerprints to use instead of the computed ones for these
    /// upstream cells (e.g. a source-verified data token or a loaded
    /// snapshot).
    pub upstream_override: &'a BTreeMap<String, Fingerprint>,
}

fn keep_comment(text: &str) -> bool {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = compact.to_ascii_lowercase();
    lower.starts_with("--sqlly")
        || lower.starts_with("--+")
        || lower.starts_with("/*+")
        || lower.starts_with("/*!")
}

/// SQL reduced to its tokens: whitespace and ordinary comments dropped,
/// keywords upper-cased; strings, identifiers, optimizer hints, executable
/// comments and `-- sqlly` directives kept verbatim.
pub fn normalize_sql(db: DatabaseType, sql: &str) -> String {
    thread_local! {
        static NORMALIZED: std::cell::RefCell<super::memo::SourceMemo> = Default::default();
    }
    NORMALIZED
        .with(|memo| super::memo::SourceMemo::get(memo, db, sql, || normalize_sql_now(db, sql)))
}

fn normalize_sql_now(db: DatabaseType, sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    for token in StrictLexer::tokenize(sql, db.family()) {
        let text = match token.kind {
            TokenKind::Whitespace => continue,
            TokenKind::Comment(_) if !keep_comment(token.text) => continue,
            TokenKind::Comment(_) => token.text.trim_end().to_string(),
            TokenKind::Keyword => token.text.to_ascii_uppercase(),
            _ => token.text.to_string(),
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&text);
    }
    out
}

/// Markdown with line endings unified and trailing blanks removed.
pub fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn source_part(db: DatabaseType, cell: &Cell) -> Fingerprint {
    let normalized = match cell.kind.source_language() {
        SourceLanguage::Sql => normalize_sql(db, &cell.source),
        _ => normalize_text(&cell.source),
    };
    let mut h = Hasher::new("source");
    h.text(&normalized);
    h.finish()
}

fn json_text(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn kind_spec_part(cell: &Cell) -> Fingerprint {
    let mut h = Hasher::new("kind_spec");
    h.text(cell.kind.as_str())
        .text(&json_text(&Sorted(&cell.spec)));
    h.finish()
}

fn connection_part(doc: &NotebookDoc, cell: &Cell, inputs: &FingerprintInputs<'_>) -> Fingerprint {
    let mut h = Hasher::new("connection");
    if cell.exec_class() != super::document::ExecClass::Remote {
        h.text("in-process");
        return h.finish();
    }
    let slot = doc.effective_connection_ref(cell);
    let key = slot.unwrap_or(DEFAULT_CONNECTION);
    let slot_database = slot
        .and_then(|s| doc.connection(s))
        .and_then(|c| c.database.clone());
    h.text(key);
    match inputs.connections.get(key) {
        Some(identity) => {
            h.text("mapped")
                .text(&identity.profile_id)
                .text(
                    identity
                        .database
                        .as_deref()
                        .or(slot_database.as_deref())
                        .unwrap_or(""),
                )
                .text(&identity.login_identity)
                .text(&identity.engine)
                .text(&json_text(&identity.policy_flags))
                .text(&json_text(&identity.session_settings));
        }
        None => {
            h.text("unmapped")
                .text(slot_database.as_deref().unwrap_or(""));
        }
    }
    h.finish()
}

fn policy_part(cell: &Cell) -> Fingerprint {
    let p = &cell.execution_policy;
    let mut h = Hasher::new("policy");
    h.text(if p.read_only {
        "read_only"
    } else {
        "read_write"
    })
    .text(&json_text(&p.max_rows))
    .text(&json_text(&p.max_download_bytes))
    .text(&json_text(&p.cache))
    .text(cell.session_group.as_deref().unwrap_or(""));
    h.finish()
}

fn params_part(graph: &Graph, cell: &Cell, params: &ResolvedParams) -> Fingerprint {
    let mut h = Hasher::new("params");
    for name in graph.params_of(&cell.id) {
        h.text(&name.to_ascii_lowercase());
        match (params.get(name), params.error(name)) {
            (Some(value), _) => {
                h.text(value.type_tag())
                    .text(&json_text(&Sorted(&value.to_json())));
            }
            (None, Some(error)) => {
                h.text("error").text(&format!("{error:?}"));
            }
            (None, None) => {
                h.text("undeclared");
            }
        }
    }
    h.finish()
}

fn combine(parts: &FingerprintParts) -> Fingerprint {
    let mut h = Hasher::new("cell");
    for part in [
        &parts.source,
        &parts.kind_spec,
        &parts.connection,
        &parts.policy,
        &parts.params,
        &parts.upstream,
    ] {
        h.text(&part.0);
    }
    h.finish()
}

/// One cell's fingerprint given its upstream cells' fingerprints.
pub fn fingerprint_cell(
    doc: &NotebookDoc,
    graph: &Graph,
    cell: &Cell,
    inputs: &FingerprintInputs<'_>,
    upstream: &BTreeMap<String, Fingerprint>,
) -> CellFingerprint {
    let db = inputs.dialects.for_cell(doc, cell);
    let mut ups: Vec<(&str, String)> = graph
        .upstream(&Node::cell(cell.id.as_str()))
        .iter()
        .filter_map(|(n, _)| n.as_cell())
        .map(|id| {
            let value = inputs
                .upstream_override
                .get(id)
                .or_else(|| upstream.get(id))
                .map(|f| f.0.clone())
                .unwrap_or_else(|| "cycle".to_string());
            (id, value)
        })
        .collect();
    ups.sort();
    ups.dedup();
    let mut h = Hasher::new("upstream");
    for (id, value) in &ups {
        h.text(id).text(value);
    }
    let parts = FingerprintParts {
        source: source_part(db, cell),
        kind_spec: kind_spec_part(cell),
        connection: connection_part(doc, cell, inputs),
        policy: policy_part(cell),
        params: params_part(graph, cell, inputs.params),
        upstream: h.finish(),
    };
    CellFingerprint {
        value: combine(&parts),
        parts,
    }
}

/// Every cell's fingerprint, upstream first. Cells on or behind a cycle
/// hash their unorderable inputs as "cycle".
pub fn fingerprint_all(
    doc: &NotebookDoc,
    graph: &Graph,
    inputs: &FingerprintInputs<'_>,
) -> BTreeMap<String, CellFingerprint> {
    let mut out: BTreeMap<String, CellFingerprint> = BTreeMap::new();
    let mut values: BTreeMap<String, Fingerprint> = BTreeMap::new();
    let by_id: std::collections::HashMap<&str, &Cell> =
        doc.cells.iter().map(|c| (c.id.as_str(), c)).collect();
    let cyclic: Vec<&Node> = graph
        .nodes()
        .iter()
        .filter(|n| graph.cyclic().contains(*n))
        .collect();
    for node in graph.order().iter().chain(cyclic) {
        let Some(cell) = node.as_cell().and_then(|id| by_id.get(id).copied()) else {
            continue;
        };
        let fp = fingerprint_cell(doc, graph, cell, inputs, &values);
        values.insert(cell.id.clone(), fp.value.clone());
        out.insert(cell.id.clone(), fp);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{CachePolicy, CellKind, ConnectionRef, Extra};
    use crate::notebook::params::{ParamType, ParamValue, ParameterDef};

    struct Fixture {
        doc: NotebookDoc,
        connections: BTreeMap<String, ConnectionIdentity>,
        params: ResolvedParams,
        overrides: BTreeMap<String, Fingerprint>,
        dialects: Dialects,
    }

    impl Fixture {
        fn new() -> Self {
            let mut doc = NotebookDoc::empty("n", "t");
            let cells = [
                (
                    "region_sales",
                    CellKind::Sql,
                    "SELECT * FROM sales WHERE region = :region",
                ),
                ("all_sales", CellKind::Sql, "SELECT * FROM sales"),
                (
                    "by_region",
                    CellKind::Sql,
                    "SELECT region, SUM(x) FROM {{region_sales}} GROUP BY region",
                ),
                ("chart", CellKind::Chart, ""),
                ("notes", CellKind::Markdown, "Total {{all_sales}}"),
                (
                    "dated",
                    CellKind::Sql,
                    "SELECT * FROM sales WHERE d >= {{param.period.start}}",
                ),
            ];
            for (id, kind, src) in cells {
                let mut c = Cell::new(id, kind, Default::default());
                c.source = src.into();
                doc.cells.push(c);
            }
            doc.cells[3].spec = serde_json::json!({"input": "by_region", "kind": "bar"});
            doc.parameters
                .push(ParameterDef::new("region", ParamType::String));
            doc.parameters
                .push(ParameterDef::new("period", ParamType::DateRange));
            let mut params = ResolvedParams::default();
            params
                .values
                .insert("region".into(), ParamValue::Text("EMEA".into()));
            params.values.insert(
                "period".into(),
                ParamValue::DateRange {
                    start: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                    end: chrono::NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(),
                },
            );
            let mut connections = BTreeMap::new();
            connections.insert(
                DEFAULT_CONNECTION.to_string(),
                ConnectionIdentity {
                    profile_id: "profile-1".into(),
                    database: Some("Sales".into()),
                    login_identity: "alice".into(),
                    engine: "SQL Server 16".into(),
                    policy_flags: BTreeSet::from(["read_only".to_string()]),
                    session_settings: BTreeMap::new(),
                },
            );
            Fixture {
                doc,
                connections,
                params,
                overrides: BTreeMap::new(),
                dialects: Dialects::single(DatabaseType::SqlServer),
            }
        }

        fn run(&self) -> BTreeMap<String, CellFingerprint> {
            let graph = Graph::build(&self.doc, &self.dialects);
            fingerprint_all(
                &self.doc,
                &graph,
                &FingerprintInputs {
                    dialects: &self.dialects,
                    connections: &self.connections,
                    params: &self.params,
                    upstream_override: &self.overrides,
                },
            )
        }
    }

    fn changed(
        a: &BTreeMap<String, CellFingerprint>,
        b: &BTreeMap<String, CellFingerprint>,
    ) -> Vec<String> {
        a.iter()
            .filter(|(id, fp)| b[*id].value != fp.value)
            .map(|(id, _)| id.clone())
            .collect()
    }

    #[test]
    fn stable_across_runs_and_hex_shaped() {
        let f = Fixture::new();
        let a = f.run();
        assert_eq!(a, f.run());
        assert_eq!(a.len(), 6);
        for fp in a.values() {
            assert_eq!(fp.value.0.len(), 64);
            assert!(fp.value.0.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_eq!(fp.value.short().len(), 12);
        }
    }

    // NB-03: changing one parameter only invalidates the cells that use it.
    #[test]
    fn a_parameter_change_only_moves_its_dependents() {
        let mut f = Fixture::new();
        let before = f.run();
        f.params
            .values
            .insert("region".into(), ParamValue::Text("APAC".into()));
        let after = f.run();
        assert_eq!(
            changed(&before, &after),
            vec!["by_region", "chart", "region_sales"]
        );
        assert_eq!(
            after["region_sales"].changes_from(&before["region_sales"]),
            vec![ChangeReason::Parameters]
        );
        assert_eq!(
            after["by_region"].changes_from(&before["by_region"]),
            vec![ChangeReason::Upstream]
        );
        let mut f2 = Fixture::new();
        f2.params.values.insert(
            "period".into(),
            ParamValue::DateRange {
                start: chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                end: chrono::NaiveDate::from_ymd_opt(2026, 2, 28).unwrap(),
            },
        );
        assert_eq!(changed(&before, &f2.run()), vec!["dated"]);
    }

    #[test]
    fn parameter_type_is_part_of_the_key() {
        let mut f = Fixture::new();
        let before = f.run();
        f.params
            .values
            .insert("region".into(), ParamValue::Integer(1));
        let a = f.run();
        f.params
            .values
            .insert("region".into(), ParamValue::Text("1".into()));
        let b = f.run();
        assert_ne!(a["region_sales"].value, b["region_sales"].value);
        assert_ne!(before["region_sales"].value, b["region_sales"].value);
    }

    #[test]
    fn whitespace_case_and_plain_comments_do_not_matter() {
        let mut f = Fixture::new();
        let before = f.run();
        f.doc.cells[1].source = "select *\r\n  from   sales -- all rows\n/* note */".into();
        assert!(changed(&before, &f.run()).is_empty());
        f.doc.cells[1].source = "SELECT * FROM Sales".into();
        assert_eq!(changed(&before, &f.run()), vec!["all_sales", "notes"]);
        f.doc.cells[1].source = "SELECT /*+ NOLOCK */ * FROM sales".into();
        assert!(!changed(&before, &f.run()).is_empty());
        f.doc.cells[1].source = "SELECT * FROM sales WHERE n = 'a  b'".into();
        let x = f.run();
        f.doc.cells[1].source = "SELECT * FROM sales WHERE n = 'a b'".into();
        assert_ne!(x["all_sales"].value, f.run()["all_sales"].value);
    }

    #[test]
    fn directives_and_hints_are_kept() {
        let db = DatabaseType::SqlServer;
        assert_eq!(normalize_sql(db, "select 1 -- note"), "SELECT 1");
        assert_eq!(
            normalize_sql(db, "select 1 -- sqlly grid: lock"),
            "SELECT 1 -- sqlly grid: lock"
        );
        assert_eq!(
            normalize_sql(DatabaseType::MySql, "select /*!50000 1 */"),
            normalize_sql(DatabaseType::MySql, "SELECT  /*!50000 1 */")
        );
        assert_eq!(normalize_text("a  \r\nb\n\n"), "a\nb");
    }

    #[test]
    fn identity_and_policy_changes_invalidate_remote_cells_only() {
        let mut f = Fixture::new();
        let before = f.run();
        f.connections
            .get_mut(DEFAULT_CONNECTION)
            .unwrap()
            .login_identity = "bob".into();
        let after = f.run();
        // Every SQL cell moves (and what reads them); Markdown with no SQL
        // upstream would not.
        assert_eq!(changed(&before, &after).len(), 6);
        assert_eq!(
            after["all_sales"].changes_from(&before["all_sales"]),
            vec![ChangeReason::Connection]
        );
        assert_eq!(
            after["chart"].changes_from(&before["chart"]),
            vec![ChangeReason::Upstream]
        );
        let mut g = Fixture::new();
        g.doc.cells[1].execution_policy.cache = CachePolicy::Pinned;
        assert_eq!(changed(&before, &g.run()), vec!["all_sales", "notes"]);
        let mut h = Fixture::new();
        h.doc.cells[3].spec = serde_json::json!({"kind": "bar", "input": "by_region"});
        assert!(
            changed(&before, &h.run()).is_empty(),
            "spec key order is irrelevant"
        );
        h.doc.cells[3].spec = serde_json::json!({"kind": "line", "input": "by_region"});
        assert_eq!(changed(&before, &h.run()), vec!["chart"]);
    }

    #[test]
    fn unmapped_slots_and_overrides() {
        let mut f = Fixture::new();
        let before = f.run();
        f.doc.connection_refs.push(ConnectionRef {
            id: "warehouse".into(),
            profile_id: None,
            label: "Warehouse".into(),
            database: Some("dw".into()),
            extra: Extra::new(),
        });
        let unmapped = f.run();
        assert_ne!(before["all_sales"].value, unmapped["all_sales"].value);
        f.connections.insert(
            "warehouse".into(),
            f.connections[DEFAULT_CONNECTION].clone(),
        );
        assert_ne!(unmapped["all_sales"].value, f.run()["all_sales"].value);
        let mut g = Fixture::new();
        g.overrides
            .insert("region_sales".into(), Fingerprint("token-1".into()));
        assert_eq!(changed(&before, &g.run()), vec!["by_region", "chart"]);
    }

    #[test]
    fn cycles_still_fingerprint() {
        let mut f = Fixture::new();
        f.doc.cells[1].source = "SELECT * FROM {{notes}}".into();
        let fps = f.run();
        assert_eq!(fps.len(), 6);
    }
}
