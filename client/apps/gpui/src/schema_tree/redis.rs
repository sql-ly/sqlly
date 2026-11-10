//! The Redis key browser: what the explorer shows under a Redis connection.
//!
//! Redis has no catalog, so the tree is built from the driver's virtual
//! commands rather than the metadata service: the server node lists the
//! logical databases (`SQLLY.DATABASES`, with key and expiry counts), a
//! database lists its keys through one bounded `SCAN` walk (`SQLLY.KEYS`,
//! never `KEYS *`) folded into folders by the connection's key delimiter,
//! and a folder re-scans its own prefix on expansion so deeper levels load
//! lazily. Leaf keys carry their type and TTL from the same walk.
//!
//! A listing can be cut short in two ways, and one note row covers both: the
//! walk stops at [`KEY_LIMIT`] keys (the server-side budget), and only the
//! first [`TREE_WINDOW`] rows of what was scanned are materialized as tree
//! nodes (the client-side window). The note keeps the whole scanned listing
//! and the walk's cursor, so **Load more** widens the window without a
//! rescan and **Scan more** continues the same `SCAN` from where it stopped.

use std::sync::Arc;

use sqlly_core::redis_keys::{effective_delimiter, group_keys};
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_services::Services;

use super::load::{LoadState, escape_id_segment};
use super::model::{NodeKind, TreeNode};

/// Keys one walk stops at: enough for a busy prefix, bounded for a huge one.
pub(crate) const KEY_LIMIT: usize = 10_000;

/// Rows of one listing materialized as tree nodes before the note row offers
/// Load more. Folders and keys alike count as rows.
pub(crate) const TREE_WINDOW: usize = 2_000;

/// The driver's `SCAN` page size. A walk keeps its last page whole so the
/// cursor it reports continues exactly after it, so the rows collected per
/// walk may run a page past the limit.
const SCAN_PAGE: usize = 1_000;

/// Rows the browser collects for one `SQLLY.KEYS limit` run: the limit plus
/// the page the walk may overshoot by. Cutting rows here would make the
/// reported cursor skip them.
pub(crate) fn listing_row_cap(limit: usize) -> usize {
    limit + 2 * SCAN_PAGE
}

/// The command the browser runs for a listing, so a test can see it.
pub(crate) fn keys_command(pattern: &str, limit: usize) -> String {
    let pattern = sqlly_intel::query::safety::redis::quote_argument(pattern.as_bytes());
    format!("SQLLY.KEYS {pattern} {limit}")
}

/// [`keys_command`] continued from an earlier walk's cursor.
pub(crate) fn keys_command_from(pattern: &str, limit: usize, cursor: &str) -> String {
    let cursor = cursor.trim();
    if cursor.is_empty() || cursor == "0" {
        return keys_command(pattern, limit);
    }
    format!("{} {cursor}", keys_command(pattern, limit))
}

/// A glob that matches exactly the keys under `prefix`.
pub(crate) fn prefix_pattern(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('*');
    out
}

/// One key as the walk reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedKey {
    pub key: String,
    pub kind: String,
    pub ttl_seconds: Option<i64>,
}

/// The rows of `SQLLY.KEYS` (`key`, `type`, `ttl_seconds`, `complete`).
pub(crate) fn parse_key_rows(rows: &[Vec<CellValue>]) -> (Vec<ScannedKey>, bool) {
    let mut complete = true;
    let keys = rows
        .iter()
        .filter_map(|row| {
            let key = match row.first()? {
                CellValue::String(s) => s.clone(),
                CellValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                _ => return None,
            };
            let kind = match row.get(1) {
                Some(CellValue::String(s)) => s.clone(),
                _ => "unknown".to_string(),
            };
            let ttl_seconds = match row.get(2) {
                Some(CellValue::I64(n)) => Some(*n),
                _ => None,
            };
            if let Some(CellValue::Bool(false)) = row.get(3) {
                complete = false;
            }
            Some(ScannedKey {
                key,
                kind,
                ttl_seconds,
            })
        })
        .collect();
    (keys, complete)
}

/// How many cluster masters a `SQLLY.KEYS` walk covered (its sixth column,
/// present only on a cluster connection); `None` on a standalone server.
pub(crate) fn listing_masters(rows: &[Vec<CellValue>]) -> Option<usize> {
    match rows.first()?.get(5)? {
        CellValue::I64(n) if *n > 0 => Some(*n as usize),
        _ => None,
    }
}

/// The masters the last cluster listing of each connection walked, so a
/// listing's note can say "across N masters".
static CLUSTER_MASTERS_SEEN: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<sqlly_core::ConnectionId, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn record_masters(profile: &ConnectionProfile, masters: Option<usize>) {
    if let Ok(mut map) = CLUSTER_MASTERS_SEEN.lock() {
        match masters {
            Some(n) => map.insert(profile.id, n),
            None => map.remove(&profile.id),
        };
    }
}

/// The masters the connection's last listing walked (`None` standalone).
pub(crate) fn masters_seen(profile: &ConnectionProfile) -> Option<usize> {
    CLUSTER_MASTERS_SEEN
        .lock()
        .ok()
        .and_then(|map| map.get(&profile.id).copied())
}

/// The cursor a `SQLLY.KEYS` walk reported (its fifth column): `None` when
/// the walk was complete, or when the rows predate the column.
pub(crate) fn listing_cursor(rows: &[Vec<CellValue>]) -> Option<String> {
    let cursor = match rows.first()?.get(4)? {
        CellValue::String(s) => s.trim().to_string(),
        CellValue::I64(n) => n.to_string(),
        _ => return None,
    };
    if cursor.is_empty() || cursor == "0" {
        None
    } else {
        Some(cursor)
    }
}

/// `1h 2m`, `45s`, `3d 4h` — the TTL a leaf shows.
pub(crate) fn ttl_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (d, h, m, s) = (
        seconds / 86_400,
        (seconds % 86_400) / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// The read command the key's type calls for. Module types answer with
/// their own readers: a time series opens charted (see
/// [`timeseries_chart_script`]), the probabilistic types with their INFO.
pub(crate) fn read_command(key: &str, kind: &str) -> String {
    let key = sqlly_intel::query::safety::redis::quote_argument(key.as_bytes());
    match kind {
        "string" => format!("GET {key}"),
        "hash" => format!("HGETALL {key}"),
        "list" => format!("LRANGE {key} 0 999"),
        "set" => format!("SMEMBERS {key}"),
        "zset" => format!("ZRANGE {key} 0 999 WITHSCORES"),
        "stream" => format!("XRANGE {key} - + COUNT 1000"),
        "ReJSON-RL" | "json" => format!("JSON.GET {key}"),
        "TSDB-TYPE" => timeseries_chart_script(&key),
        "MBbloom--" => format!("BF.INFO {key}"),
        "MBbloomCF" => format!("CF.INFO {key}"),
        "TopK-TYPE" => format!("TOPK.LIST {key} WITHCOUNT"),
        "CMSk-TYPE" => format!("CMS.INFO {key}"),
        "TDIS-TYPE" => format!("TDIGEST.INFO {key}"),
        _ => format!("TYPE {key}"),
    }
}

/// A time series opens on the Chart tab with the timestamp on the X axis:
/// the `-- sqlly` directive lines are comments to the Redis script parser
/// and view hints to the results pane. `key` is already quoted.
pub(crate) fn timeseries_chart_script(key: &str) -> String {
    format!(
        "-- sqlly view: chart\n\
         -- sqlly chart: kind:line; label:timestamp; values:value; order:source\n\
         TS.RANGE {key} - +"
    )
}

// ---------------------------------------------------------------------------
// Search indexes (the search module's FT.* commands)
// ---------------------------------------------------------------------------

/// The listing the Indexes folder runs: `FT._LIST` with each index's key
/// type, prefixes and document count.
pub(crate) const INDEXES_COMMAND: &str = "SQLLY.INDEXES";

/// Which servers answered the search-module probe this session, by profile
/// id. The probe is `SQLLY.INDEXES` itself: a refusal (no module, no
/// permission, old server) hides the Indexes folder for the rest of the
/// session, so a plain Redis server never pays for a second attempt.
static SEARCH_MODULE_PROBE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<sqlly_core::ConnectionId, bool>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn probe_result(profile: &ConnectionProfile) -> Option<bool> {
    SEARCH_MODULE_PROBE
        .lock()
        .ok()
        .and_then(|map| map.get(&profile.id).copied())
}

fn record_probe(profile: &ConnectionProfile, available: bool) {
    if let Ok(mut map) = SEARCH_MODULE_PROBE.lock() {
        map.insert(profile.id, available);
    }
}

/// Forget a server's probe answer (tests, or a reconnect that may have
/// loaded the module).
#[cfg(test)]
pub(crate) fn forget_search_probe(profile: &ConnectionProfile) {
    if let Ok(mut map) = SEARCH_MODULE_PROBE.lock() {
        map.remove(&profile.id);
    }
}

/// One search index as `SQLLY.INDEXES` reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchIndex {
    pub name: String,
    /// `HASH` or `JSON`, when `FT.INFO` answered.
    pub on: Option<String>,
    /// Space-separated key prefixes the index covers.
    pub prefixes: Option<String>,
    pub num_docs: Option<i64>,
}

/// The rows of `SQLLY.INDEXES` (`index`, `on`, `prefixes`, `num_docs`).
pub(crate) fn parse_index_rows(rows: &[Vec<CellValue>]) -> Vec<SearchIndex> {
    rows.iter()
        .filter_map(|row| {
            let name = match row.first()? {
                CellValue::String(s) => s.clone(),
                CellValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                _ => return None,
            };
            let text = |i: usize| match row.get(i) {
                Some(CellValue::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            let num_docs = match row.get(3) {
                Some(CellValue::I64(n)) => Some(*n),
                _ => None,
            };
            Some(SearchIndex {
                name,
                on: text(1),
                prefixes: text(2),
                num_docs,
            })
        })
        .collect()
}

/// An index node's detail: document count, key type and prefixes.
fn index_detail(docs: Option<i64>, on: &Option<String>, prefixes: &Option<String>) -> String {
    let mut parts = Vec::new();
    if let Some(docs) = docs {
        parts.push(crate::localization::format(
            "explorer.schema_redis.index_document_count",
            &[("count", &docs.to_string())],
        ));
    }
    match (on, prefixes) {
        (Some(on), Some(prefixes)) => parts.push(crate::localization::format(
            "explorer.schema_redis.index_type_and_prefixes",
            &[("kind", on), ("prefixes", prefixes)],
        )),
        (Some(on), None) => parts.push(on.clone()),
        (None, Some(prefixes)) => parts.push(crate::localization::format(
            "explorer.schema_redis.index_prefixes",
            &[("prefixes", prefixes)],
        )),
        (None, None) => {}
    }
    parts.join(" · ")
}

/// The "Indexes" folder under a database node.
pub(crate) fn indexes_folder_node(parent_id: &str, depth: usize, count: usize) -> TreeNode {
    let mut node = TreeNode::new(
        format!("{parent_id}:ftidx"),
        crate::localization::text("explorer.schema_redis.indexes_folder"),
        NodeKind::RedisIndexesFolder,
        depth,
    );
    node.set_localized_detail(move || match count {
        0 => crate::localization::text("explorer.schema_redis.indexes_empty"),
        1 => crate::localization::text("explorer.schema_redis.indexes_one"),
        n => crate::localization::format(
            "explorer.schema_redis.indexes_count",
            &[("count", &n.to_string())],
        ),
    });
    node
}

/// Index nodes under the folder, each with its document count, key type
/// and prefixes as the detail.
pub(crate) fn index_nodes(parent_id: &str, depth: usize, indexes: &[SearchIndex]) -> Vec<TreeNode> {
    indexes
        .iter()
        .map(|index| {
            let mut node = TreeNode::new(
                format!("{parent_id}:ix:{}", escape_id_segment(&index.name)),
                index.name.clone(),
                NodeKind::RedisIndex {
                    name: index.name.clone(),
                },
                depth,
            );
            node.load_state = LoadState::Loaded;
            if index.num_docs.is_some() || index.on.is_some() || index.prefixes.is_some() {
                let (docs, on, prefixes) =
                    (index.num_docs, index.on.clone(), index.prefixes.clone());
                node.set_localized_detail(move || index_detail(docs, &on, &prefixes));
            }
            node
        })
        .collect()
}

fn quoted(name: &str) -> String {
    sqlly_intel::query::safety::redis::quote_argument(name.as_bytes())
}

/// Open: the index's definition and statistics.
pub(crate) fn index_info_command(name: &str) -> String {
    format!("FT.INFO {}", quoted(name))
}

/// Query…: a match-everything search over the first hundred documents,
/// ready to edit.
pub(crate) fn index_query_script(name: &str) -> String {
    format!("FT.SEARCH {} \"*\" LIMIT 0 100", quoted(name))
}

/// Explain…: the query plan for a query (edit the `*`).
pub(crate) fn index_explain_script(name: &str) -> String {
    format!(
        "# Replace * with the query to explain.\nFT.EXPLAIN {} \"*\"",
        quoted(name)
    )
}

/// Profile…: timing per iterator and result processor for a query.
pub(crate) fn index_profile_script(name: &str) -> String {
    format!(
        "# Replace * with the query to profile.\nFT.PROFILE {} SEARCH QUERY \"*\"",
        quoted(name)
    )
}

/// Drop…: the reviewed script. `FT.DROPINDEX` is a destructive command, so
/// running it goes through the usual confirmation; the documents stay
/// unless the user appends `DD`.
pub(crate) fn index_drop_script(name: &str) -> String {
    format!(
        "# Drops the search index {name}. The indexed documents are kept;\n\
         # append DD to delete them with it. Running this asks for confirmation.\n\
         FT.DROPINDEX {}",
        quoted(name),
        name = quoted(name)
    )
}

/// `SQLLY.INDEXES` against one logical database.
async fn list_indexes(
    services: &Services,
    profile: &ConnectionProfile,
    database: &str,
) -> Result<Vec<SearchIndex>, String> {
    let mut profile = profile.clone();
    if !database.is_empty() {
        profile.database = Some(database.to_string());
    }
    let outcome = crate::tool_dialog::execute_collect(services, &profile, INDEXES_COMMAND).await;
    if let Some(error) = outcome.error {
        return Err(error);
    }
    let rows = outcome
        .sets
        .first()
        .map(|s| s.rows.clone())
        .unwrap_or_default();
    Ok(parse_index_rows(&rows))
}

/// The Indexes folder for a database node, or `None` on a server whose
/// probe failed (silently: the folder is simply absent).
async fn indexes_folder(
    services: &Services,
    profile: &ConnectionProfile,
    database: &str,
    node_id: &str,
    depth: usize,
) -> Option<TreeNode> {
    if probe_result(profile) == Some(false) {
        return None;
    }
    match list_indexes(services, profile, database).await {
        Ok(indexes) => {
            record_probe(profile, true);
            Some(indexes_folder_node(node_id, depth, indexes.len()))
        }
        Err(_) => {
            record_probe(profile, false);
            None
        }
    }
}

/// Elements the fixed readers stop at (`LRANGE 0 999`, `ZRANGE 0 999`,
/// `XRANGE … COUNT 1000`).
pub(crate) const READER_CAP: usize = 1_000;

/// The tab title for a key's reader: a type whose reader is capped names
/// the cap (`orders — first 1,000`) so a long list is never mistaken for
/// the whole value; uncapped readers keep the tab's automatic name.
pub(crate) fn read_tab_title(key: &str, kind: &str) -> Option<String> {
    match kind {
        "list" | "zset" | "stream" => Some(crate::localization::format(
            "explorer.schema_redis.read_tab_title",
            &[("key", key), ("count", &thousands(READER_CAP))],
        )),
        _ => None,
    }
}

/// `12,345`.
pub(crate) fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Keys each memory-analyzer report samples when the menu builds its script.
pub(crate) const MEMORY_ANALYSIS_SAMPLE: usize = 1000;
/// Rows the analyzer's top-keys report lists.
pub(crate) const MEMORY_ANALYSIS_TOP: usize = 25;

/// The memory analyzer's report script: three read-only reports, each over
/// its own bounded sample of the logical database the tab targets. The
/// header comment is the honest-bounds note the user edits to widen the
/// sample or lengthen the top list.
pub(crate) fn memory_analysis_script() -> String {
    let sample = MEMORY_ANALYSIS_SAMPLE;
    let top = MEMORY_ANALYSIS_TOP;
    format!(
        "# Memory analysis of this logical database — a summary and three reports over\n\
         # ONE bounded sample per run: SCAN (never KEYS *) of up to {sample} keys (at\n\
         # most 50000), with TYPE, TTL and MEMORY USAGE … SAMPLES 5 read per sampled\n\
         # key. Commands with the same sample size share that sample, so the reports\n\
         # agree; reruns take a fresh one. sampled_* columns are measured; estimated_*\n\
         # columns scale the sample linearly to the key count (the summary's complete\n\
         # column says whether they are exact). Without MEMORY USAGE (unknown command\n\
         # or NOPERM) byte columns stay empty, the top-keys list is empty, and the\n\
         # summary says why. Raise {sample} to widen the sample, change {top} for a\n\
         # longer top-keys list. SQLLY.MEMBYPREFIX groups by key prefix: its second\n\
         # number is the depth (1-3) and its last argument the delimiters to split on\n\
         # (for example \":|\").\n\
         SQLLY.MEMSUMMARY {sample}\n\
         SQLLY.MEMBYTYPE {sample}\n\
         SQLLY.TOPKEYS {sample} {top}\n\
         SQLLY.TTLBUCKETS {sample}\n\
         SQLLY.MEMBYPREFIX {sample} 1 :"
    )
}

/// Everything one listing has scanned so far, retained on its note row so
/// Load more and Scan more extend it in place instead of starting over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisListing {
    /// The MATCH pattern the walk scans with (`*`, the database's key
    /// filter, or a folder's exact-prefix glob).
    pub pattern: String,
    /// The folder prefix the keys fold under (`""` for the database).
    pub parent_prefix: String,
    /// Every key scanned so far, in walk order.
    pub keys: Vec<ScannedKey>,
    /// The `SCAN` cursor to continue from; `None` once the walk came back
    /// to 0.
    pub cursor: Option<String>,
    /// Rows (folders + keys) the listing folds into.
    pub rows: usize,
    /// Rows materialized as tree nodes.
    pub shown: usize,
    /// Cluster masters the walk covered; `None` on a standalone server.
    pub masters: Option<usize>,
}

impl RedisListing {
    pub fn new(pattern: String, parent_prefix: String) -> Self {
        Self {
            pattern,
            parent_prefix,
            keys: Vec::new(),
            cursor: None,
            rows: 0,
            shown: TREE_WINDOW,
            masters: None,
        }
    }

    /// The walk stopped at its budget; a further walk can continue.
    pub fn scan_incomplete(&self) -> bool {
        self.cursor.is_some()
    }

    /// Rows scanned but not materialized.
    pub fn hidden_rows(&self) -> usize {
        self.rows.saturating_sub(self.shown)
    }

    /// Widen the window by one more [`TREE_WINDOW`] of rows.
    pub fn load_more(&mut self) {
        self.shown = self.shown.saturating_add(TREE_WINDOW);
    }

    /// Fold a continued walk's page in: its keys join the listing, its
    /// cursor replaces the old one, and the window grows so the new rows
    /// are visible rather than hidden behind a second Load more.
    pub fn extend(&mut self, keys: Vec<ScannedKey>, cursor: Option<String>) {
        self.keys.extend(keys);
        self.cursor = cursor;
        self.load_more();
    }
}

/// The note row's text: what is shown of what was scanned, and whether the
/// walk itself stopped short, in one sentence.
pub(crate) fn note_text(listing: &RedisListing) -> String {
    let hidden = listing.hidden_rows();
    let scanned = thousands(listing.keys.len());
    // On a cluster the walk covers every master in turn.
    let across = match listing.masters {
        Some(1) => crate::localization::text("explorer.schema_redis.across_one_master"),
        Some(n) => crate::localization::format(
            "explorer.schema_redis.across_masters",
            &[("count", &n.to_string())],
        ),
        None => String::new(),
    };
    let (key, arguments) = match (hidden > 0, listing.scan_incomplete()) {
        (true, true) => (
            "explorer.schema_redis.note_hidden_incomplete",
            vec![
                ("shown", thousands(listing.shown)),
                ("rows", thousands(listing.rows)),
                ("scanned", scanned),
                ("across", across),
            ],
        ),
        (true, false) => (
            "explorer.schema_redis.note_hidden_complete",
            vec![
                ("shown", thousands(listing.shown)),
                ("rows", thousands(listing.rows)),
                ("across", across),
            ],
        ),
        (false, true) => (
            "explorer.schema_redis.note_scan_incomplete",
            vec![("scanned", scanned), ("across", across)],
        ),
        (false, false) => return String::new(),
    };
    let arguments = arguments
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect::<Vec<_>>();
    crate::localization::format(key, &arguments)
}

/// Swap a database or folder's key listing for a widened one while keeping
/// the rows that are not part of the listing (the search Indexes folder),
/// so Load more and Scan more never drop it.
pub(crate) fn replace_listing_children(children: &mut Vec<TreeNode>, fresh: Vec<TreeNode>) {
    let mut kept: Vec<TreeNode> = children
        .drain(..)
        .filter(|child| matches!(child.kind, NodeKind::RedisIndexesFolder))
        .collect();
    kept.extend(fresh);
    *children = kept;
}

/// Fold a listing into folder and key nodes under `parent_id`, materializing
/// only the listing's window and ending with the note row whenever anything
/// — scanned rows or unscanned keys — is left over.
pub(crate) fn nodes_for_window(
    parent_id: &str,
    depth: usize,
    delimiter: &str,
    mut listing: RedisListing,
) -> Vec<TreeNode> {
    let names: Vec<String> = listing.keys.iter().map(|k| k.key.clone()).collect();
    let grouping = group_keys(&names, &listing.parent_prefix, delimiter);
    let complete = !listing.scan_incomplete();
    let mut nodes = Vec::with_capacity(grouping.folders.len() + grouping.leaves.len() + 1);
    for folder in grouping.folders {
        let mut node = TreeNode::new(
            format!("{parent_id}:kf:{}", escape_id_segment(&folder.prefix)),
            folder.label.clone(),
            NodeKind::RedisKeyFolder {
                prefix: folder.prefix.clone(),
            },
            depth,
        );
        let key = if complete {
            "explorer.schema_redis.folder_key_count"
        } else {
            "explorer.schema_redis.folder_key_count_incomplete"
        };
        let count = folder.count;
        node.set_localized_detail(move || {
            crate::localization::format(key, &[("count", &count.to_string())])
        });
        nodes.push(node);
    }
    for index in grouping.leaves {
        let key = &listing.keys[index];
        let mut node = TreeNode::new(
            format!("{parent_id}:k:{}", escape_id_segment(&key.key)),
            key.key.clone(),
            NodeKind::RedisKey {
                key: key.key.clone(),
                kind: key.kind.clone(),
                ttl_seconds: key.ttl_seconds,
            },
            depth,
        );
        node.load_state = LoadState::Loaded;
        node.detail = Some(match key.ttl_seconds {
            Some(ttl) => format!("{} · {}", key.kind, ttl_text(ttl)),
            None => key.kind.clone(),
        });
        nodes.push(node);
    }
    listing.rows = nodes.len();
    nodes.truncate(listing.shown);
    if listing.hidden_rows() > 0 || listing.scan_incomplete() {
        let mut note = TreeNode::new(
            format!("{parent_id}:note"),
            note_text(&listing),
            NodeKind::RedisNote {
                listing: Arc::new(listing),
            },
            depth,
        );
        note.load_state = LoadState::Loaded;
        nodes.push(note);
    }
    nodes
}

/// The database node's detail: key count, plus how many of them expire.
pub(crate) fn database_detail(keys: i64, expires: i64) -> String {
    if keys == 0 {
        crate::localization::text("explorer.schema_redis.database_empty")
    } else if expires > 0 {
        crate::localization::format(
            "explorer.schema_redis.database_keys_expiring",
            &[
                ("count", &keys.to_string()),
                ("expires", &expires.to_string()),
            ],
        )
    } else {
        crate::localization::format(
            "explorer.schema_redis.database_keys",
            &[("count", &keys.to_string())],
        )
    }
}

/// How a logical database is named on this profile: `db3`, or
/// `db3 — sessions` when the profile names it.
pub(crate) fn database_label(db: i64, aliases: &std::collections::BTreeMap<i64, String>) -> String {
    match aliases.get(&db).map(|a| a.trim()).filter(|a| !a.is_empty()) {
        Some(alias) => format!("db{db} — {alias}"),
        None => format!("db{db}"),
    }
}

/// [`database_label`] for a database named by its text (`"3"`); anything
/// that is not a number keeps its own spelling.
pub(crate) fn database_label_text(db: &str, profile: &ConnectionProfile) -> String {
    match db.trim().parse::<i64>() {
        Ok(n) => database_label(n, &profile.redis_db_aliases),
        Err(_) => format!("db{db}"),
    }
}

/// Parse the "show only databases" field: numbers separated by commas or
/// spaces, ranges like `0-3`; blank means every database. Duplicates are
/// dropped, the typed order is kept.
// Used by the native key-action dialog only.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn parse_visible_databases(text: &str) -> Result<Option<Vec<i64>>, String> {
    let mut out: Vec<i64> = Vec::new();
    for part in text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
    {
        let bad = || {
            crate::localization::format(
                "explorer.schema_redis.invalid_visible_database",
                &[("value", &format!("\"{part}\""))],
            )
        };
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (
                a.parse::<i64>().map_err(|_| bad())?,
                b.parse::<i64>().map_err(|_| bad())?,
            ),
            None => {
                let n = part.parse::<i64>().map_err(|_| bad())?;
                (n, n)
            }
        };
        if lo < 0 || hi < lo || hi - lo > 10_000 {
            return Err(bad());
        }
        for n in lo..=hi {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    Ok((!out.is_empty()).then_some(out))
}

/// The visible-databases list as the editor shows it.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn visible_databases_text(visible: Option<&[i64]>) -> String {
    visible
        .map(|v| v.iter().map(i64::to_string).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// The server node's children from `SQLLY.DATABASES` rows: one node per
/// logical database, labelled with the profile's names, limited to (and
/// ordered by) the profile's visible-databases list when it has one.
pub(crate) fn database_nodes(
    node_id: &str,
    child_depth: usize,
    rows: &[Vec<CellValue>],
    profile: &ConnectionProfile,
) -> Vec<TreeNode> {
    let mut nodes: Vec<(i64, TreeNode)> = rows
        .iter()
        .filter_map(|row| {
            let name = match row.first()? {
                CellValue::String(s) => s.clone(),
                CellValue::I64(n) => n.to_string(),
                _ => return None,
            };
            let number = name.trim().parse::<i64>().unwrap_or(i64::MAX);
            let int = |i: usize| match row.get(i) {
                Some(CellValue::I64(n)) => *n,
                _ => 0,
            };
            let mut node = TreeNode::new(
                format!("{node_id}:db:{}", escape_id_segment(&name)),
                database_label_text(&name, profile),
                NodeKind::Database(name),
                child_depth,
            );
            // No counts when the server refused INFO: no detail rather
            // than a false "empty".
            if !matches!(row.get(1), Some(CellValue::Null)) {
                let (keys, expires) = (int(1), int(2));
                node.set_localized_detail(move || database_detail(keys, expires));
            }
            Some((number, node))
        })
        .collect();
    if let Some(visible) = profile
        .redis_visible_databases
        .as_ref()
        .filter(|v| !v.is_empty())
    {
        nodes.retain(|(n, _)| visible.contains(n));
        nodes.sort_by_key(|(n, _)| visible.iter().position(|v| v == n));
    }
    nodes.into_iter().map(|(_, node)| node).collect()
}

/// Children of a Redis node. `filter` is the database node's MATCH pattern.
pub(crate) async fn load_redis_children(
    services: &Services,
    profile: &ConnectionProfile,
    kind: &NodeKind,
    node_id: &str,
    child_depth: usize,
    filter: Option<&str>,
) -> Result<Vec<TreeNode>, String> {
    let delimiter = effective_delimiter(profile);
    match kind {
        NodeKind::Server { .. } => {
            let outcome =
                crate::tool_dialog::execute_collect(services, profile, "SQLLY.DATABASES").await;
            if let Some(error) = outcome.error {
                return Err(error);
            }
            let rows = outcome
                .sets
                .first()
                .map(|s| s.rows.clone())
                .unwrap_or_default();
            Ok(database_nodes(node_id, child_depth, &rows, profile))
        }
        NodeKind::Database(db) => {
            let pattern = filter
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .unwrap_or("*")
                .to_string();
            let mut listing = RedisListing::new(pattern.clone(), String::new());
            let (keys, cursor) = scan(services, profile, db, &pattern, "0").await?;
            listing.keys = keys;
            listing.cursor = cursor;
            listing.masters = masters_seen(profile);
            // Search indexes first (only on a server with the module),
            // then the key folders and keys.
            let mut nodes = Vec::new();
            if let Some(folder) = indexes_folder(services, profile, db, node_id, child_depth).await
            {
                nodes.push(folder);
            }
            nodes.extend(nodes_for_window(node_id, child_depth, &delimiter, listing));
            Ok(nodes)
        }
        NodeKind::RedisIndexesFolder => {
            let db = super::load::extract_db_from_id(node_id);
            let indexes = list_indexes(services, profile, &db).await?;
            Ok(index_nodes(node_id, child_depth, &indexes))
        }
        NodeKind::RedisKeyFolder { prefix } => {
            let db = super::load::extract_db_from_id(node_id);
            let pattern = prefix_pattern(prefix);
            let mut listing = RedisListing::new(pattern.clone(), prefix.clone());
            let (keys, cursor) = scan(services, profile, &db, &pattern, "0").await?;
            listing.keys = keys;
            listing.cursor = cursor;
            listing.masters = masters_seen(profile);
            Ok(nodes_for_window(node_id, child_depth, &delimiter, listing))
        }
        _ => Ok(Vec::new()),
    }
}

/// One `SQLLY.KEYS` walk of `pattern` in `database`, from `cursor` (`"0"`
/// starts fresh). Returns the keys and the cursor to continue from (`None`
/// when the walk completed).
pub(crate) async fn scan(
    services: &Services,
    profile: &ConnectionProfile,
    database: &str,
    pattern: &str,
    cursor: &str,
) -> Result<(Vec<ScannedKey>, Option<String>), String> {
    let mut profile = profile.clone();
    if !database.is_empty() {
        profile.database = Some(database.to_string());
    }
    let outcome = crate::tool_dialog::execute_collect_with_cap(
        services,
        &profile,
        &keys_command_from(pattern, KEY_LIMIT, cursor),
        listing_row_cap(KEY_LIMIT),
    )
    .await;
    if let Some(error) = outcome.error {
        return Err(error);
    }
    let rows = outcome
        .sets
        .first()
        .map(|s| s.rows.clone())
        .unwrap_or_default();
    let (keys, complete) = parse_key_rows(&rows);
    if !rows.is_empty() {
        record_masters(&profile, listing_masters(&rows));
    }
    let cursor = if complete {
        None
    } else {
        // An incomplete walk that lost its cursor (rows from an older
        // driver) is still incomplete; "0" would claim otherwise, so keep a
        // cursor that restarts the walk rather than pretending it finished.
        Some(listing_cursor(&rows).unwrap_or_else(|| "0".to_string()))
    };
    Ok((keys, cursor))
}

/// The key names a Redis database or folder load produced, for command
/// IntelliSense; `None` for any other node (or a scan that found no keys).
pub(crate) fn redis_key_load_payload(
    kind: &NodeKind,
    children: &[TreeNode],
) -> Option<Vec<sqlly_intel::redis_complete::KnownKey>> {
    if !matches!(
        kind,
        NodeKind::Database(_) | NodeKind::RedisKeyFolder { .. }
    ) {
        return None;
    }
    let keys: Vec<_> = children
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::RedisKey { key, kind, .. } => Some(sqlly_intel::redis_complete::KnownKey {
                name: key.clone(),
                kind: kind.clone(),
            }),
            _ => None,
        })
        .collect();
    (!keys.is_empty()).then_some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widening_a_listing_keeps_the_indexes_folder() {
        let folder = indexes_folder_node("srv:x:db:0", 2, 1);
        let mut children = vec![folder];
        let mut listing = RedisListing::new("*".into(), String::new());
        listing.keys = vec![key("a", "string", None)];
        replace_listing_children(
            &mut children,
            nodes_for_window("srv:x:db:0", 2, ":", listing),
        );
        assert!(matches!(children[0].kind, NodeKind::RedisIndexesFolder));
        assert!(children.iter().any(|c| c.title == "a"));
    }

    #[test]
    fn key_load_payload_collects_leaf_keys_of_redis_listings_only() {
        let keys = vec![
            key("user:1", "hash", None),
            key("user:2", "hash", Some(5)),
            key("order:1", "string", None),
        ];
        let mut top = RedisListing::new("*".into(), String::new());
        top.keys = keys.clone();
        let nodes = nodes_for_window("srv:x:db:0", 2, ":", top);
        // Folders only at the top level: no leaf keys yet.
        assert!(redis_key_load_payload(&NodeKind::Database("0".into()), &nodes).is_none());
        let mut user = RedisListing::new("user:*".into(), "user:".into());
        user.keys = keys[..2].to_vec();
        let under_user = nodes_for_window("srv:x:db:0:f:user", 3, ":", user);
        let payload = redis_key_load_payload(
            &NodeKind::RedisKeyFolder {
                prefix: "user:".into(),
            },
            &under_user,
        )
        .expect("leaf keys");
        let names: Vec<&str> = payload.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, vec!["user:1", "user:2"]);
        assert_eq!(payload[0].kind, "hash");
        assert!(
            redis_key_load_payload(
                &NodeKind::Server {
                    id: uuid::Uuid::nil()
                },
                &under_user
            )
            .is_none()
        );
    }

    fn key(name: &str, kind: &str, ttl: Option<i64>) -> ScannedKey {
        ScannedKey {
            key: name.into(),
            kind: kind.into(),
            ttl_seconds: ttl,
        }
    }

    fn listing_of(keys: Vec<ScannedKey>, cursor: Option<&str>, shown: usize) -> RedisListing {
        RedisListing {
            pattern: "*".into(),
            parent_prefix: String::new(),
            keys,
            cursor: cursor.map(str::to_string),
            rows: 0,
            shown,
            masters: None,
        }
    }

    #[test]
    fn cluster_listing_note_says_across_how_many_masters() {
        let mut listing = listing_of(
            vec![key("a", "string", None), key("b", "string", None)],
            Some("127.0.0.1:7101@88"),
            10,
        );
        listing.masters = Some(3);
        let text = note_text(&listing);
        assert!(text.contains("2 keys across 3 masters"), "{text}");
        listing.masters = None;
        assert!(!note_text(&listing).contains("masters"));
        let row = |masters: Option<i64>| {
            let mut row = vec![
                CellValue::String("k".into()),
                CellValue::String("string".into()),
                CellValue::Null,
                CellValue::Bool(false),
                CellValue::String("127.0.0.1:7101@88".into()),
            ];
            if let Some(n) = masters {
                row.push(CellValue::I64(n));
            }
            row
        };
        assert_eq!(listing_masters(&[row(Some(3))]), Some(3));
        assert_eq!(
            listing_masters(&[row(None)]),
            None,
            "standalone rows have no column"
        );
        assert_eq!(
            listing_cursor(&[row(Some(3))]).as_deref(),
            Some("127.0.0.1:7101@88"),
            "a cluster cursor survives as text"
        );
    }

    #[test]
    fn commands_quote_patterns_and_pick_the_reader_by_type() {
        assert_eq!(keys_command("*", 10), "SQLLY.KEYS * 10");
        assert_eq!(keys_command("user:*", 5), "SQLLY.KEYS user:* 5");
        assert_eq!(keys_command("a b*", 5), "SQLLY.KEYS \"a b*\" 5");
        assert_eq!(keys_command_from("user:*", 5, "0"), "SQLLY.KEYS user:* 5");
        assert_eq!(keys_command_from("user:*", 5, ""), "SQLLY.KEYS user:* 5");
        assert_eq!(
            keys_command_from("user:*", 5, "1234"),
            "SQLLY.KEYS user:* 5 1234"
        );
        assert_eq!(prefix_pattern("user:"), "user:*");
        assert_eq!(prefix_pattern("a[b:"), "a\\[b:*");
        assert_eq!(read_command("k", "string"), "GET k");
        assert_eq!(read_command("h", "hash"), "HGETALL h");
        assert_eq!(read_command("l", "list"), "LRANGE l 0 999");
        assert_eq!(read_command("s", "set"), "SMEMBERS s");
        assert_eq!(read_command("z", "zset"), "ZRANGE z 0 999 WITHSCORES");
        assert_eq!(read_command("x", "stream"), "XRANGE x - + COUNT 1000");
        assert_eq!(read_command("j", "ReJSON-RL"), "JSON.GET j");
        assert_eq!(
            read_command("say \"hi\"", "string"),
            "GET \"say \\\"hi\\\"\""
        );
        assert_eq!(read_command("k", "whatever"), "TYPE k");
        assert_eq!(read_command("b", "MBbloom--"), "BF.INFO b");
        assert_eq!(read_command("c", "MBbloomCF"), "CF.INFO c");
        assert_eq!(read_command("t", "TopK-TYPE"), "TOPK.LIST t WITHCOUNT");
        assert_eq!(read_command("m", "CMSk-TYPE"), "CMS.INFO m");
        assert_eq!(read_command("d", "TDIS-TYPE"), "TDIGEST.INFO d");
    }

    #[test]
    fn time_series_open_charted_with_the_timestamp_on_x() {
        let script = read_command("sensor 1", "TSDB-TYPE");
        assert_eq!(
            script,
            "-- sqlly view: chart\n-- sqlly chart: kind:line; label:timestamp; values:value; order:source\nTS.RANGE \"sensor 1\" - +"
        );
        // The directive lines are comments to the Redis parser: only the
        // TS.RANGE runs.
        let commands = sqlly_intel::query::safety::redis::parse_script(&script).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name(), "TS.RANGE");
        // And view hints to the results pane: the chart opens with
        // `timestamp` as the label and `value` as the series.
        let hints = crate::result_pivot::parse_view_hints(&script);
        assert_eq!(
            hints.default_view,
            crate::result_pivot::ResultViewKind::Chart
        );
        let chart = hints.chart.expect("chart hint");
        let columns = [
            sqlly_core::ResultColumn {
                name: "timestamp".into(),
                data_type: "datetime".into(),
                nullable: true,
            },
            sqlly_core::ResultColumn {
                name: "value".into(),
                data_type: "double".into(),
                nullable: true,
            },
            sqlly_core::ResultColumn {
                name: "timestamp_ms".into(),
                data_type: "integer".into(),
                nullable: true,
            },
        ];
        let config = chart.chart_config(&columns).expect("columns resolve");
        assert_eq!(config.label_column, Some(0));
        assert_eq!(config.value_columns, vec![1]);
    }

    #[test]
    fn index_scripts_quote_names_and_only_the_drop_is_destructive() {
        use sqlly_intel::query::safety::redis::{CommandAccess, command_access, parse_script};
        assert_eq!(index_info_command("idx"), "FT.INFO idx");
        assert_eq!(index_query_script("idx"), "FT.SEARCH idx \"*\" LIMIT 0 100");
        assert_eq!(
            index_query_script("my idx"),
            "FT.SEARCH \"my idx\" \"*\" LIMIT 0 100"
        );
        assert!(index_explain_script("idx").ends_with("FT.EXPLAIN idx \"*\""));
        assert!(index_profile_script("idx").ends_with("FT.PROFILE idx SEARCH QUERY \"*\""));
        let drop = index_drop_script("idx");
        assert!(drop.ends_with("FT.DROPINDEX idx"), "{drop}");
        // The comment mentions DD; the command itself never carries it.
        assert_eq!(
            drop.lines().last(),
            Some("FT.DROPINDEX idx"),
            "documents are kept unless the user asks"
        );
        for (script, expected) in [
            (index_query_script("idx"), CommandAccess::ReadOnly),
            (index_explain_script("idx"), CommandAccess::ReadOnly),
            (index_profile_script("idx"), CommandAccess::ReadOnly),
            (drop.clone(), CommandAccess::Destructive),
        ] {
            let commands = parse_script(&script).unwrap();
            assert_eq!(commands.len(), 1, "{script}");
            assert_eq!(command_access(&commands[0]), expected, "{script}");
        }
    }

    #[test]
    fn index_rows_become_nodes_with_docs_type_and_prefixes() {
        let rows = vec![
            vec![
                CellValue::String("idx:users".into()),
                CellValue::String("JSON".into()),
                CellValue::String("user: account:".into()),
                CellValue::I64(1200),
            ],
            vec![
                CellValue::String("idx:bare".into()),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
            ],
        ];
        let indexes = parse_index_rows(&rows);
        assert_eq!(indexes.len(), 2);
        assert_eq!(indexes[0].on.as_deref(), Some("JSON"));
        assert_eq!(indexes[0].num_docs, Some(1200));
        assert_eq!(
            indexes[1],
            SearchIndex {
                name: "idx:bare".into(),
                on: None,
                prefixes: None,
                num_docs: None
            }
        );
        let nodes = index_nodes("srv:x:db:0:ftidx", 3, &indexes);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id, "srv:x:db:0:ftidx:ix:idx%3Ausers");
        assert!(matches!(&nodes[0].kind, NodeKind::RedisIndex { name } if name == "idx:users"));
        assert_eq!(
            nodes[0].detail.as_deref(),
            Some("1200 docs · JSON on user: account:")
        );
        assert_eq!(nodes[1].detail, None);
        assert!(!nodes[0].kind.is_expandable());
        let folder = indexes_folder_node("srv:x:db:0", 2, 2);
        assert_eq!(folder.id, "srv:x:db:0:ftidx");
        assert!(matches!(folder.kind, NodeKind::RedisIndexesFolder));
        assert!(folder.kind.is_expandable());
        assert_eq!(folder.detail.as_deref(), Some("2 indexes"));
        assert_eq!(
            indexes_folder_node("p", 2, 0).detail.as_deref(),
            Some("none yet")
        );
        assert_eq!(
            indexes_folder_node("p", 2, 1).detail.as_deref(),
            Some("1 index")
        );
    }

    #[test]
    fn search_probe_is_remembered_per_server() {
        let profile = ConnectionProfile::sql_login(
            "r",
            "h",
            "u",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        forget_search_probe(&profile);
        assert_eq!(probe_result(&profile), None);
        record_probe(&profile, false);
        assert_eq!(probe_result(&profile), Some(false));
        record_probe(&profile, true);
        assert_eq!(probe_result(&profile), Some(true));
        forget_search_probe(&profile);
        assert_eq!(probe_result(&profile), None);
    }

    #[test]
    fn capped_readers_name_their_cap_in_the_tab_title() {
        assert_eq!(
            read_tab_title("orders", "list").as_deref(),
            Some("orders — first 1,000")
        );
        assert_eq!(
            read_tab_title("board", "zset").as_deref(),
            Some("board — first 1,000")
        );
        assert_eq!(
            read_tab_title("events", "stream").as_deref(),
            Some("events — first 1,000")
        );
        // Whole-value readers keep the automatic tab name.
        assert_eq!(read_tab_title("k", "string"), None);
        assert_eq!(read_tab_title("h", "hash"), None);
        assert_eq!(read_tab_title("s", "set"), None);
        assert_eq!(read_tab_title("j", "ReJSON-RL"), None);
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn memory_analysis_script_is_bounded_and_honest() {
        let script = memory_analysis_script();
        assert!(script.contains("SQLLY.MEMSUMMARY 1000"));
        assert!(script.contains("SQLLY.MEMBYTYPE 1000"));
        assert!(script.contains("SQLLY.TOPKEYS 1000 25"));
        assert!(script.contains("ONE bounded sample"), "{script}");
        assert!(script.contains("SQLLY.TTLBUCKETS 1000"));
        assert!(script.contains("SQLLY.MEMBYPREFIX 1000 1 :"), "{script}");
        assert!(script.contains("never KEYS *"), "{script}");
        assert!(script.contains("estimated_"), "{script}");
        // Comment lines lead the report; the three commands follow.
        assert!(script.starts_with('#'), "{script}");
    }

    #[test]
    fn ttl_text_is_compact() {
        assert_eq!(ttl_text(45), "45s");
        assert_eq!(ttl_text(125), "2m 5s");
        assert_eq!(ttl_text(3720), "1h 2m");
        assert_eq!(ttl_text(90_000), "1d 1h");
    }

    #[test]
    fn parses_key_rows_and_notices_truncation() {
        let rows = vec![
            vec![
                CellValue::String("user:1".into()),
                CellValue::String("hash".into()),
                CellValue::Null,
                CellValue::Bool(true),
            ],
            vec![
                CellValue::Bytes(vec![0xff]),
                CellValue::String("string".into()),
                CellValue::I64(30),
                CellValue::Bool(false),
            ],
        ];
        let (keys, complete) = parse_key_rows(&rows);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], key("user:1", "hash", None));
        assert_eq!(keys[1].kind, "string");
        assert_eq!(keys[1].ttl_seconds, Some(30));
        assert!(!complete);
        // Rows without the cursor column (an older driver) carry none.
        assert_eq!(listing_cursor(&rows), None);
    }

    #[test]
    fn reads_the_cursor_column_and_treats_zero_as_done() {
        let row = |cursor: CellValue| {
            vec![vec![
                CellValue::String("k".into()),
                CellValue::String("string".into()),
                CellValue::Null,
                CellValue::Bool(false),
                cursor,
            ]]
        };
        assert_eq!(
            listing_cursor(&row(CellValue::String("8832".into()))).as_deref(),
            Some("8832")
        );
        assert_eq!(
            listing_cursor(&row(CellValue::I64(17))).as_deref(),
            Some("17")
        );
        assert_eq!(listing_cursor(&row(CellValue::String("0".into()))), None);
        assert_eq!(listing_cursor(&row(CellValue::String(" ".into()))), None);
        assert_eq!(listing_cursor(&[]), None);
    }

    #[test]
    fn listing_becomes_folders_then_keys_then_a_note() {
        let keys = vec![
            key("user:1:profile", "hash", None),
            key("user:2:profile", "hash", None),
            key("order:9", "list", Some(60)),
            key("config", "string", Some(3600)),
        ];
        let nodes = nodes_for_window(
            "srv:x:db:0",
            2,
            ":",
            listing_of(keys.clone(), Some("42"), 10),
        );
        assert_eq!(nodes.len(), 4);
        assert!(matches!(&nodes[0].kind, NodeKind::RedisKeyFolder { prefix } if prefix == "user:"));
        assert_eq!(nodes[0].title, "user");
        assert_eq!(nodes[0].detail.as_deref(), Some("2+ keys"));
        assert_eq!(nodes[0].id, "srv:x:db:0:kf:user%3A");
        assert!(
            matches!(&nodes[1].kind, NodeKind::RedisKeyFolder { prefix } if prefix == "order:")
        );
        assert!(
            matches!(&nodes[2].kind, NodeKind::RedisKey { key, kind, ttl_seconds: Some(3600) } if key == "config" && kind == "string")
        );
        assert_eq!(nodes[2].detail.as_deref(), Some("string · 1h 0m"));
        let NodeKind::RedisNote { listing } = &nodes[3].kind else {
            panic!("the walk stopped short, so a note row follows");
        };
        assert_eq!(listing.cursor.as_deref(), Some("42"));
        assert_eq!(listing.rows, 3);
        assert_eq!(listing.hidden_rows(), 0);
        assert_eq!(
            nodes[3].title,
            "SCAN stopped at 4 keys — click to scan more, or use Filter Keys… to narrow"
        );
        // A folder's own listing folds under its prefix; a complete walk
        // with everything shown has no note at all.
        let mut inner = listing_of(keys, None, 10);
        inner.parent_prefix = "user:".into();
        let inner = nodes_for_window("srv:x:db:0:kf:user%3A", 3, ":", inner);
        assert_eq!(inner.len(), 2);
        assert!(
            inner
                .iter()
                .all(|n| matches!(n.kind, NodeKind::RedisKeyFolder { .. }))
        );
    }

    #[test]
    fn the_window_clips_rows_and_the_note_says_how_many_wait() {
        let keys: Vec<ScannedKey> = (0..50)
            .map(|i| key(&format!("k{i:02}"), "string", None))
            .collect();
        // A complete walk, clipped by the window: the note offers Load more
        // and nothing else.
        let nodes = nodes_for_window("srv:x:db:0", 2, ":", listing_of(keys.clone(), None, 20));
        assert_eq!(nodes.len(), 21, "20 rows plus the note");
        assert!(
            nodes[..20]
                .iter()
                .all(|n| matches!(n.kind, NodeKind::RedisKey { .. }))
        );
        let NodeKind::RedisNote { listing } = &nodes[20].kind else {
            panic!("note row");
        };
        assert_eq!(listing.rows, 50);
        assert_eq!(listing.hidden_rows(), 30);
        assert!(!listing.scan_incomplete());
        assert_eq!(
            nodes[20].title,
            "Showing 20 of 50 rows — click to load more"
        );

        // Both truncations at once: one note names both.
        let nodes = nodes_for_window(
            "srv:x:db:0",
            2,
            ":",
            listing_of(keys.clone(), Some("9"), 20),
        );
        assert_eq!(
            nodes[20].title,
            "Showing 20 of 50 rows from 50 scanned keys; the SCAN stopped there — click to load more, Scan more to continue, or Filter Keys… to narrow"
        );

        // Load more widens the window without touching the keys; once every
        // row is shown and the walk is complete the note disappears.
        let NodeKind::RedisNote { listing } = &nodes[20].kind else {
            panic!("note row");
        };
        let mut widened = (**listing).clone();
        widened.cursor = None;
        widened.load_more();
        assert_eq!(widened.shown, 20 + TREE_WINDOW);
        let nodes = nodes_for_window("srv:x:db:0", 2, ":", widened);
        assert_eq!(nodes.len(), 50);
        assert!(
            nodes
                .iter()
                .all(|n| !matches!(n.kind, NodeKind::RedisNote { .. }))
        );
    }

    #[test]
    fn scan_more_appends_the_continued_page_and_keeps_the_new_cursor() {
        let mut listing = listing_of(vec![key("a:1", "string", None)], Some("7"), 1);
        listing.extend(
            vec![key("a:2", "string", None), key("b", "list", None)],
            Some("99".to_string()),
        );
        assert_eq!(listing.keys.len(), 3);
        assert_eq!(listing.cursor.as_deref(), Some("99"));
        assert_eq!(listing.shown, 1 + TREE_WINDOW, "the new rows are visible");
        listing.extend(vec![key("c", "set", None)], None);
        assert!(!listing.scan_incomplete());
        let nodes = nodes_for_window("srv:x:db:0", 2, ":", listing);
        // a: folder, b, c — the whole continued walk, no note left.
        assert_eq!(nodes.len(), 3);
        assert!(matches!(&nodes[0].kind, NodeKind::RedisKeyFolder { prefix } if prefix == "a:"));
        assert_eq!(nodes[0].detail.as_deref(), Some("2 keys"));
    }

    #[test]
    fn database_detail_counts_keys_and_expiries() {
        assert_eq!(database_detail(0, 0), "empty");
        assert_eq!(database_detail(120, 0), "120 keys");
        assert_eq!(database_detail(120, 7), "120 keys · expires: 7");
    }

    #[test]
    fn listing_row_cap_allows_the_walk_its_last_page() {
        assert!(listing_row_cap(KEY_LIMIT) > KEY_LIMIT + SCAN_PAGE);
    }

    #[test]
    fn redis_menus_use_the_unified_expiry_and_filter_wording() {
        use super::super::ExplorerActionContext;
        use super::super::menus::{MenuAvailability, MenuEntry, redis_menu_entries};
        let labels = |kind: NodeKind| -> Vec<String> {
            let ctx = ExplorerActionContext {
                node_id: "srv:x:db:0".into(),
                title: "db0".into(),
                kind,
                server_id: None,
                database: Some("0".into()),
                schema: None,
                name: None,
                table: None,
                copy_name: None,
                copy_qualified: None,
                columns: Vec::new(),
                profile: None,
                position: gpui::Point::new(gpui::px(0.0), gpui::px(0.0)),
            };
            let availability = MenuAvailability {
                has_connection: true,
                can_mutate: true,
                ..Default::default()
            };
            // Through submenus too: expiry lives under the key's Expiry group.
            fn walk(entries: &[MenuEntry], out: &mut Vec<String>) {
                for entry in entries {
                    match entry {
                        MenuEntry::Action { label, .. } => out.push(label.to_string()),
                        MenuEntry::ActionOwned { label, .. } => out.push(label.clone()),
                        MenuEntry::SubmenuOwned { entries, .. } => walk(entries, out),
                        _ => {}
                    }
                }
            }
            let mut out = Vec::new();
            walk(&redis_menu_entries(&ctx, &availability), &mut out);
            out
        };
        let database = labels(NodeKind::Database("0".into()));
        assert!(
            database.iter().any(|l| l == "Clear Key Filter"),
            "{database:?}"
        );
        assert!(
            !database.iter().any(|l| l == "Show All Keys"),
            "{database:?}"
        );
        assert!(database.iter().any(|l| l == "Scan more"), "{database:?}");
        let key = labels(NodeKind::RedisKey {
            key: "k".into(),
            kind: "string".into(),
            ttl_seconds: Some(5),
        });
        assert!(
            key.iter().any(|l| l == "Remove expiry (PERSIST)"),
            "{key:?}"
        );
        assert!(!key.iter().any(|l| l.contains("Remove TTL")), "{key:?}");
        let folder = labels(NodeKind::RedisKeyFolder {
            prefix: "user:".into(),
        });
        assert!(folder.iter().any(|l| l == "Scan more"), "{folder:?}");
        let note = labels(NodeKind::RedisNote {
            listing: Arc::new(listing_of(vec![], Some("3"), 1)),
        });
        assert!(note.iter().any(|l| l == "Load more"), "{note:?}");
        assert!(note.iter().any(|l| l == "Scan more"), "{note:?}");
    }
}

#[cfg(test)]
mod aliases {
    use super::*;

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "cache",
            "127.0.0.1",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Redis;
        p
    }

    fn db_rows(n: i64) -> Vec<Vec<CellValue>> {
        (0..n)
            .map(|i| {
                vec![
                    CellValue::String(i.to_string()),
                    CellValue::I64(i * 10),
                    CellValue::I64(0),
                    CellValue::I64(0),
                ]
            })
            .collect()
    }

    #[test]
    fn database_labels_use_the_profile_names() {
        let mut p = profile();
        p.redis_db_aliases.insert(3, "sessions".into());
        p.redis_db_aliases.insert(4, "   ".into());
        assert_eq!(database_label(3, &p.redis_db_aliases), "db3 — sessions");
        assert_eq!(database_label(4, &p.redis_db_aliases), "db4");
        assert_eq!(database_label(0, &p.redis_db_aliases), "db0");
        assert_eq!(database_label_text(" 3", &p), "db3 — sessions");

        let nodes = database_nodes("srv", 1, &db_rows(5), &p);
        let titles: Vec<&str> = nodes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["db0", "db1", "db2", "db3 — sessions", "db4"]);
        // The node keeps the bare number: the alias is display only.
        assert!(matches!(&nodes[3].kind, NodeKind::Database(name) if name == "3"));
        assert_eq!(nodes[3].id, "srv:db:3");
    }

    #[test]
    fn databases_without_counts_show_no_detail_rather_than_empty() {
        // The rows SQLLY.DATABASES gives a user denied INFO.
        let rows: Vec<Vec<CellValue>> = (0..3)
            .map(|i| {
                vec![
                    CellValue::String(i.to_string()),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Null,
                ]
            })
            .collect();
        let nodes = database_nodes("srv", 1, &rows, &profile());
        assert_eq!(nodes.len(), 3);
        assert!(nodes.iter().all(|n| n.detail.is_none()));
        let counted = database_nodes("srv", 1, &db_rows(2), &profile());
        assert_eq!(counted[0].detail.as_deref(), Some("empty"));
    }

    #[test]
    fn visible_databases_filter_and_order_the_list() {
        let mut p = profile();
        p.redis_visible_databases = Some(vec![5, 0, 3, 42]);
        let nodes = database_nodes("srv", 1, &db_rows(16), &p);
        let titles: Vec<&str> = nodes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["db5", "db0", "db3"], "42 does not exist");
        p.redis_visible_databases = Some(Vec::new());
        assert_eq!(database_nodes("srv", 1, &db_rows(3), &p).len(), 3);
    }

    #[test]
    fn visible_databases_field_parses_lists_and_ranges() {
        assert_eq!(parse_visible_databases(""), Ok(None));
        assert_eq!(parse_visible_databases(" 0, 3 5"), Ok(Some(vec![0, 3, 5])));
        assert_eq!(
            parse_visible_databases("0-2,1,7"),
            Ok(Some(vec![0, 1, 2, 7]))
        );
        assert!(parse_visible_databases("a").is_err());
        assert!(parse_visible_databases("3-1").is_err());
        assert!(parse_visible_databases("-2").is_err());
        assert_eq!(visible_databases_text(Some(&[0, 3])), "0, 3");
        assert_eq!(visible_databases_text(None), "");
    }

    #[test]
    fn aliases_survive_the_profile_round_trip() {
        let mut p = profile();
        p.redis_db_aliases.insert(3, "sessions".into());
        p.redis_visible_databases = Some(vec![0, 3]);
        let json = serde_json::to_string(&p).unwrap();
        let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.redis_db_aliases, p.redis_db_aliases);
        assert_eq!(back.redis_visible_databases, Some(vec![0, 3]));
        // Profiles without names keep the fields out of the file.
        let plain = serde_json::to_string(&profile()).unwrap();
        assert!(!plain.contains("redis_db_aliases"), "{plain}");
        assert!(!plain.contains("redis_visible_databases"), "{plain}");
    }
}
