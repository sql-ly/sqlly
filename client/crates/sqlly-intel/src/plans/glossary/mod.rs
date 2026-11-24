//! Operator glossary: what every operator an engine can print does, what it
//! means for performance, and which cross-engine category it belongs to.
//!
//! The category drives icons and colours in both plan viewers; the operator
//! key (`postgres.bitmap_heap_scan`) is the stable handle article links and
//! tests hang off. Descriptions are written per engine on purpose: operators
//! with similar names do different work on different engines.

mod clickhouse;
mod cockroachdb;
mod duckdb;
mod mysql;
mod oracle;
mod postgres;
mod spark;
mod sqlite;
mod sqlserver;
mod trino;

use std::collections::HashMap;
use std::sync::LazyLock;

use sqlly_core::DisplayMessage;

use crate::plans::dialects::PlanDialect;
use crate::plans::import::PlanEngine;
use crate::plans::model::PlanNode;
use crate::plans::warning::PlanWarning;

/// Cross-engine operator family. `key()` is stable: renderers pick icons and
/// colours by it and article links are keyed on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OperatorCategory {
    /// Reads a whole table or a whole index.
    Scan,
    /// Reads part of an index (or a table by key) using a search condition.
    Seek,
    /// Fetches the rest of a row from the table after an index found it.
    Lookup,
    JoinNestedLoop,
    JoinHash,
    JoinMerge,
    /// Joins whose algorithm is none of the three above (cross products,
    /// range and as-of joins) or is not stated by the plan.
    Join,
    Sort,
    /// GROUP BY, DISTINCT and plain aggregates.
    Aggregate,
    Window,
    /// Saves rows for reuse: spools, materialization, temp tables, hash builds.
    Spool,
    /// Moves rows between threads or processes.
    Parallelism,
    Filter,
    /// Evaluates expressions or reshapes rows without reading or combining.
    Compute,
    /// INSERT, UPDATE, DELETE, MERGE and DDL-style writers.
    Modify,
    /// Work done on another server.
    Remote,
    /// UNION, INTERSECT, EXCEPT and plain concatenation.
    SetOp,
    /// TOP, LIMIT, OFFSET and other row-count stops.
    Limit,
    /// Subqueries, CTEs, derived tables and views run as their own unit.
    Subquery,
    /// Table functions, unnest and other row generators.
    Function,
    /// Rows that come from constants rather than a table.
    Constant,
    /// The statement itself or the step that hands rows to the client.
    Result,
    /// Row locking.
    Lock,
    Other,
}

impl OperatorCategory {
    pub const ALL: [OperatorCategory; 24] = [
        Self::Scan,
        Self::Seek,
        Self::Lookup,
        Self::JoinNestedLoop,
        Self::JoinHash,
        Self::JoinMerge,
        Self::Join,
        Self::Sort,
        Self::Aggregate,
        Self::Window,
        Self::Spool,
        Self::Parallelism,
        Self::Filter,
        Self::Compute,
        Self::Modify,
        Self::Remote,
        Self::SetOp,
        Self::Limit,
        Self::Subquery,
        Self::Function,
        Self::Constant,
        Self::Result,
        Self::Lock,
        Self::Other,
    ];

    /// Stable identifier (`join_nested_loop`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Seek => "seek",
            Self::Lookup => "lookup",
            Self::JoinNestedLoop => "join_nested_loop",
            Self::JoinHash => "join_hash",
            Self::JoinMerge => "join_merge",
            Self::Join => "join",
            Self::Sort => "sort",
            Self::Aggregate => "aggregate",
            Self::Window => "window",
            Self::Spool => "spool",
            Self::Parallelism => "parallelism",
            Self::Filter => "filter",
            Self::Compute => "compute",
            Self::Modify => "modify",
            Self::Remote => "remote",
            Self::SetOp => "set_op",
            Self::Limit => "limit",
            Self::Subquery => "subquery",
            Self::Function => "function",
            Self::Constant => "constant",
            Self::Result => "result",
            Self::Lock => "lock",
            Self::Other => "other",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|category| category.key() == key)
    }

    pub fn is_join(self) -> bool {
        matches!(
            self,
            Self::JoinNestedLoop | Self::JoinHash | Self::JoinMerge | Self::Join
        )
    }

    /// Reads rows from storage (scan, seek or lookup).
    pub fn is_access(self) -> bool {
        matches!(self, Self::Scan | Self::Seek | Self::Lookup)
    }

    /// Short label for legends and filters.
    pub fn label(self) -> DisplayMessage {
        let (key, fallback) = match self {
            Self::Scan => ("services.plans.categories.scan", "Scan"),
            Self::Seek => ("services.plans.categories.seek", "Seek"),
            Self::Lookup => ("services.plans.categories.lookup", "Lookup"),
            Self::JoinNestedLoop => (
                "services.plans.categories.join_nested_loop",
                "Nested loop join",
            ),
            Self::JoinHash => ("services.plans.categories.join_hash", "Hash join"),
            Self::JoinMerge => ("services.plans.categories.join_merge", "Merge join"),
            Self::Join => ("services.plans.categories.join", "Join"),
            Self::Sort => ("services.plans.categories.sort", "Sort"),
            Self::Aggregate => ("services.plans.categories.aggregate", "Aggregate"),
            Self::Window => ("services.plans.categories.window", "Window function"),
            Self::Spool => ("services.plans.categories.spool", "Spool / materialize"),
            Self::Parallelism => ("services.plans.categories.parallelism", "Parallelism"),
            Self::Filter => ("services.plans.categories.filter", "Filter"),
            Self::Compute => ("services.plans.categories.compute", "Compute"),
            Self::Modify => ("services.plans.categories.modify", "Data modification"),
            Self::Remote => ("services.plans.categories.remote", "Remote"),
            Self::SetOp => ("services.plans.categories.set_op", "Set operation"),
            Self::Limit => ("services.plans.categories.limit", "Limit"),
            Self::Subquery => ("services.plans.categories.subquery", "Subquery / CTE"),
            Self::Function => ("services.plans.categories.function", "Function"),
            Self::Constant => ("services.plans.categories.constant", "Constant rows"),
            Self::Result => ("services.plans.categories.result", "Result"),
            Self::Lock => ("services.plans.categories.lock", "Locking"),
            Self::Other => ("services.plans.categories.other", "Other"),
        };
        DisplayMessage::new(key, fallback)
    }
}

/// One glossary row. Built with the [`op!`] macro so the translation keys
/// always follow from the operator key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorEntry {
    /// `engine.operator`, for example `postgres.bitmap_heap_scan`.
    pub key: &'static str,
    /// The operator as the engine prints it; the first name is the display
    /// name, the rest are aliases (older versions, other plan formats).
    pub names: &'static [&'static str],
    pub category: OperatorCategory,
    pub description_key: &'static str,
    pub description: &'static str,
    pub performance_key: &'static str,
    pub performance: &'static str,
}

impl OperatorEntry {
    pub fn display_name(&self) -> &'static str {
        self.names.first().copied().unwrap_or(self.key)
    }
}

/// `op!("postgres", "seq_scan", ["Seq Scan"], Scan, "description", "performance")`.
macro_rules! op {
    ($engine:literal, $id:literal, [$($name:literal),+ $(,)?], $category:ident, $description:literal, $performance:literal $(,)?) => {
        $crate::plans::glossary::OperatorEntry {
            key: concat!($engine, ".", $id),
            names: &[$($name),+],
            category: $crate::plans::glossary::OperatorCategory::$category,
            description_key: concat!("services.plans.glossary.", $engine, ".", $id, ".description"),
            description: $description,
            performance_key: concat!("services.plans.glossary.", $engine, ".", $id, ".performance"),
            performance: $performance,
        }
    };
}
pub(crate) use op;

/// What the glossary says about one operator of a parsed plan.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorInfo {
    /// Stable operator key (`sqlserver.key_lookup`); `engine.unknown` when
    /// the glossary has no entry for the operator.
    pub key: &'static str,
    /// The operator name as the engine prints it, without object names.
    pub name: String,
    pub category: OperatorCategory,
    /// One line: what the operator does.
    pub description: DisplayMessage,
    /// When it is fine and when it is a smell.
    pub performance: DisplayMessage,
    /// False when the entry is the generic fallback.
    pub known: bool,
}

/// Glossary id of an engine (`sqlserver`, `postgres`, `mysql`, `oracle`,
/// `sqlite`, `duckdb`, `clickhouse`). MySQL and MariaDB share one vocabulary.
pub fn engine_key(dialect: PlanDialect) -> &'static str {
    match dialect {
        PlanDialect::SqlServer => "sqlserver",
        PlanDialect::Postgres => "postgres",
        PlanDialect::MySql | PlanDialect::MariaDb => "mysql",
        PlanDialect::Oracle => "oracle",
        PlanDialect::Sqlite => "sqlite",
        PlanDialect::DuckDb => "duckdb",
        PlanDialect::ClickHouse => "clickhouse",
    }
}

/// Glossary id of an engine. The same ids as [`engine_key`] for the engines
/// that have a dialect, plus `cockroachdb`, `trino` and `spark`.
pub fn engine_key_for(engine: PlanEngine) -> &'static str {
    match engine {
        PlanEngine::CockroachDb => "cockroachdb",
        PlanEngine::Trino => "trino",
        PlanEngine::Spark => "spark",
        other => engine_key(other.analysis_dialect()),
    }
}

/// Every glossary entry for `engine`.
pub fn operator_glossary_for(engine: PlanEngine) -> &'static [OperatorEntry] {
    match engine {
        PlanEngine::CockroachDb => cockroachdb::OPERATORS,
        PlanEngine::Trino => trino::OPERATORS,
        PlanEngine::Spark => spark::OPERATORS,
        other => operator_glossary(other.analysis_dialect()),
    }
}

/// Every glossary entry for `dialect`'s engine.
pub fn operator_glossary(dialect: PlanDialect) -> &'static [OperatorEntry] {
    match dialect {
        PlanDialect::SqlServer => sqlserver::OPERATORS,
        PlanDialect::Postgres => postgres::OPERATORS,
        PlanDialect::MySql | PlanDialect::MariaDb => mysql::OPERATORS,
        PlanDialect::Oracle => oracle::OPERATORS,
        PlanDialect::Sqlite => sqlite::OPERATORS,
        PlanDialect::DuckDb => duckdb::OPERATORS,
        PlanDialect::ClickHouse => clickhouse::OPERATORS,
    }
}

/// Every glossary table, one per engine vocabulary.
pub fn all_operator_glossaries() -> [(&'static str, &'static [OperatorEntry]); 10] {
    [
        ("sqlserver", sqlserver::OPERATORS),
        ("postgres", postgres::OPERATORS),
        ("mysql", mysql::OPERATORS),
        ("oracle", oracle::OPERATORS),
        ("sqlite", sqlite::OPERATORS),
        ("duckdb", duckdb::OPERATORS),
        ("clickhouse", clickhouse::OPERATORS),
        ("cockroachdb", cockroachdb::OPERATORS),
        ("trino", trino::OPERATORS),
        ("spark", spark::OPERATORS),
    ]
}

/// Entry for an operator key (`duckdb.hash_join`).
pub fn operator_entry(key: &str) -> Option<&'static OperatorEntry> {
    all_operator_glossaries()
        .into_iter()
        .flat_map(|(_, entries)| entries.iter())
        .find(|entry| entry.key == key)
}

// Placeholders until the three tables land in their own files.

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

struct EngineIndex {
    /// Lower-cased operator name -> entry.
    by_name: HashMap<String, &'static OperatorEntry>,
}

fn engine_index(engine: PlanEngine) -> &'static EngineIndex {
    static INDEXES: LazyLock<[EngineIndex; 10]> = LazyLock::new(|| {
        all_operator_glossaries().map(|(_, entries)| {
            let mut by_name = HashMap::with_capacity(entries.len() * 2);
            for entry in entries {
                for name in entry.names {
                    by_name.entry(name.to_ascii_lowercase()).or_insert(entry);
                }
            }
            EngineIndex { by_name }
        })
    });
    // The order of `all_operator_glossaries`.
    let slot = match engine {
        PlanEngine::SqlServer => 0,
        PlanEngine::Postgres => 1,
        PlanEngine::MySql | PlanEngine::MariaDb => 2,
        PlanEngine::Oracle => 3,
        PlanEngine::Sqlite => 4,
        PlanEngine::DuckDb => 5,
        PlanEngine::ClickHouse => 6,
        PlanEngine::CockroachDb => 7,
        PlanEngine::Trino => 8,
        PlanEngine::Spark => 9,
    };
    &INDEXES[slot]
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The entry whose name equals `text`, else the entry with the longest name
/// that is a prefix of `text` ending at a word boundary. ASCII
/// case-insensitive.
fn lookup_text(engine: PlanEngine, text: &str) -> Option<&'static OperatorEntry> {
    let index = engine_index(engine);
    let lowered: String = text
        .trim()
        .chars()
        .take(160)
        .collect::<String>()
        .to_ascii_lowercase();
    if lowered.is_empty() {
        return None;
    }
    if let Some(entry) = index.by_name.get(lowered.as_str()) {
        return Some(entry);
    }
    // Candidate cut points, longest first: before a non-word character and
    // right after one (so names ending in punctuation such as `Select #`
    // still match `Select #2`).
    let chars: Vec<(usize, char)> = lowered.char_indices().collect();
    for position in (1..chars.len()).rev() {
        let (offset, ch) = chars[position];
        let previous = chars[position - 1].1;
        if is_word_char(ch) && is_word_char(previous) {
            continue;
        }
        let candidate = lowered[..offset].trim_end();
        if candidate.is_empty() {
            continue;
        }
        if let Some(entry) = index.by_name.get(candidate) {
            return Some(entry);
        }
    }
    None
}

/// Glossary lookup by operator text alone (no plan node), for pasted
/// operator names and tests. `logical` is the logical operator when the
/// engine has one (SQL Server), else empty.
pub fn operator_lookup(
    dialect: PlanDialect,
    physical: &str,
    logical: &str,
) -> Option<&'static OperatorEntry> {
    operator_lookup_for(PlanEngine::from_dialect(dialect), physical, logical)
}

/// [`operator_lookup`] for any engine plans can be imported from.
pub fn operator_lookup_for(
    engine: PlanEngine,
    physical: &str,
    logical: &str,
) -> Option<&'static OperatorEntry> {
    candidates(engine, physical, logical, None)
        .iter()
        .find_map(|candidate| lookup_text(engine, candidate))
}

/// What the glossary says about `node`'s operator. Unknown operators come
/// back with `known == false`, the key `<engine>.unknown`, a category guessed
/// from the operator's name and a neutral description.
pub fn operator_info(dialect: PlanDialect, node: &PlanNode) -> OperatorInfo {
    operator_match_for(PlanEngine::from_dialect(dialect), node).0
}

/// [`operator_info`] for any engine plans can be imported from. CockroachDB,
/// Trino and Spark operators are looked up in their own glossaries and come
/// back as `cockroachdb.unknown`, `trino.unknown` or `spark.unknown` when
/// the glossary has no entry.
pub fn operator_info_for(engine: PlanEngine, node: &PlanNode) -> OperatorInfo {
    operator_match_for(engine, node).0
}

/// [`operator_info`] plus the glossary row it came from, so callers can tell
/// which of the row's names the operator text started with.
pub(crate) fn operator_match_for(
    engine: PlanEngine,
    node: &PlanNode,
) -> (OperatorInfo, Option<&'static OperatorEntry>) {
    let texts = candidates(engine, &node.physical_op, &node.logical_op, Some(node));
    if let Some(entry) = texts
        .iter()
        .find_map(|candidate| lookup_text(engine, candidate))
    {
        let info = OperatorInfo {
            key: entry.key,
            name: entry.display_name().to_string(),
            category: entry.category,
            description: DisplayMessage::new(entry.description_key, entry.description),
            performance: DisplayMessage::new(entry.performance_key, entry.performance),
            known: true,
        };
        return (info, Some(entry));
    }
    let name = texts
        .last()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| node.logical_op.trim().to_string());
    let info = OperatorInfo {
        key: unknown_key(engine),
        category: guess_category(&name),
        description: DisplayMessage::new(
            "services.plans.glossary.unknown.description",
            format!("{name}: this operator is not in the glossary yet."),
        )
        .with_arg("operator", name.clone()),
        performance: DisplayMessage::new(
            "services.plans.glossary.unknown.performance",
            "Judge it by its share of the plan's time or cost and by its row counts.",
        ),
        name,
        known: false,
    };
    (info, None)
}

fn unknown_key(engine: PlanEngine) -> &'static str {
    match engine {
        PlanEngine::SqlServer => "sqlserver.unknown",
        PlanEngine::Postgres => "postgres.unknown",
        PlanEngine::MySql | PlanEngine::MariaDb => "mysql.unknown",
        PlanEngine::Oracle => "oracle.unknown",
        PlanEngine::Sqlite => "sqlite.unknown",
        PlanEngine::DuckDb => "duckdb.unknown",
        PlanEngine::ClickHouse => "clickhouse.unknown",
        PlanEngine::CockroachDb => "cockroachdb.unknown",
        PlanEngine::Trino => "trino.unknown",
        PlanEngine::Spark => "spark.unknown",
    }
}

/// Operator texts to try, most specific first. The last one is the plain
/// operator name used for display when nothing matches.
fn candidates(
    engine: PlanEngine,
    physical: &str,
    logical: &str,
    node: Option<&PlanNode>,
) -> Vec<String> {
    let physical = physical.trim();
    let logical = logical.trim();
    let base = if physical.is_empty() {
        logical
    } else {
        physical
    };
    let mut out = Vec::with_capacity(3);
    let dialect = match engine {
        // These print one operator name per node and nothing to canonicalize.
        PlanEngine::CockroachDb | PlanEngine::Trino | PlanEngine::Spark => {
            out.push(base.to_string());
            return out;
        }
        other => other.analysis_dialect(),
    };
    match dialect {
        PlanDialect::SqlServer => {
            let is_lookup = node.is_some_and(|node| {
                node.property("Lookup")
                    .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                    || node
                        .warnings
                        .iter()
                        .any(|warning| matches!(warning, PlanWarning::KeyLookup { .. }))
            });
            if is_lookup && base.eq_ignore_ascii_case("Clustered Index Seek") {
                out.push("Key Lookup".to_string());
            }
            if !logical.is_empty() && !logical.eq_ignore_ascii_case(base) {
                out.push(format!("{base} ({logical})"));
            }
            out.push(base.to_string());
        }
        PlanDialect::Postgres => {
            let stripped = strip_postgres_decorations(base);
            if let Some(node) = node {
                if stripped.eq_ignore_ascii_case("Aggregate") {
                    match node.property("Strategy") {
                        Some(value) if value.eq_ignore_ascii_case("Hashed") => {
                            out.push("HashAggregate".to_string());
                        }
                        Some(value) if value.eq_ignore_ascii_case("Sorted") => {
                            out.push("GroupAggregate".to_string());
                        }
                        Some(value) if value.eq_ignore_ascii_case("Mixed") => {
                            out.push("MixedAggregate".to_string());
                        }
                        _ => {}
                    }
                } else if stripped.eq_ignore_ascii_case("ModifyTable")
                    && let Some(operation) = node.property("Operation")
                {
                    out.push(operation.to_string());
                } else if stripped.eq_ignore_ascii_case("SetOp")
                    && node
                        .property("Strategy")
                        .is_some_and(|value| value.eq_ignore_ascii_case("Hashed"))
                {
                    out.push("HashSetOp".to_string());
                }
            }
            out.push(stripped);
        }
        PlanDialect::MySql | PlanDialect::MariaDb => {
            // The JSON parsers put the table in the physical operator and
            // the access type in the logical one.
            if base
                .get(..6)
                .is_some_and(|head| head.eq_ignore_ascii_case("Table:"))
                && !logical.is_empty()
            {
                out.push(logical.to_string());
            } else {
                if !logical.is_empty() && !logical.eq_ignore_ascii_case(base) {
                    out.push(base.to_string());
                    out.push(logical.to_string());
                }
                out.push(base.to_string());
            }
        }
        PlanDialect::Sqlite => {
            out.push(canonical_sqlite(base));
            out.push(base.to_string());
        }
        PlanDialect::ClickHouse => {
            out.push(base.to_string());
            // One entry covers the whole `ReadFromSystem<Table>` family.
            const SYSTEM: &str = "ReadFromSystem";
            if base.len() > SYSTEM.len()
                && base
                    .get(..SYSTEM.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(SYSTEM))
            {
                out.push(SYSTEM.to_string());
                out.push(base.to_string());
            }
        }
        PlanDialect::Oracle | PlanDialect::DuckDb => {
            out.push(base.to_string());
        }
    }
    out
}

/// `Parallel Seq Scan` -> `Seq Scan`, `Partial HashAggregate` ->
/// `HashAggregate`, `Index Scan Backward` -> `Index Scan`, and the join-type
/// spellings (`Hash Left Join`, `Nested Loop Semi Join`) -> the plain join.
fn strip_postgres_decorations(text: &str) -> String {
    let mut rest = text.trim();
    loop {
        let mut changed = false;
        for prefix in ["Parallel ", "Partial ", "Finalize ", "Simple "] {
            if rest.len() > prefix.len()
                && rest
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            {
                rest = &rest[prefix.len()..];
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let words: Vec<&str> = rest.split(' ').collect();
    let join_word = |word: &str| {
        ["Left", "Right", "Full", "Semi", "Anti", "Inner", "Outer"]
            .iter()
            .any(|candidate| word.eq_ignore_ascii_case(candidate))
    };
    let lowered = rest.to_ascii_lowercase();
    if lowered.starts_with("nested loop") {
        let tail_is_join = words.len() >= 3
            && words[2..]
                .iter()
                .take_while(|word| !word.eq_ignore_ascii_case("Join"))
                .all(|word| join_word(word));
        if tail_is_join
            && words[2..]
                .iter()
                .any(|word| word.eq_ignore_ascii_case("Join"))
        {
            let after: Vec<&str> = words
                .iter()
                .skip_while(|word| !word.eq_ignore_ascii_case("Join"))
                .skip(1)
                .copied()
                .collect();
            return join_words("Nested Loop", &after);
        }
    }
    for algorithm in ["Hash", "Merge"] {
        if words.len() >= 3
            && words[0].eq_ignore_ascii_case(algorithm)
            && words[1..]
                .iter()
                .take_while(|word| !word.eq_ignore_ascii_case("Join"))
                .all(|word| join_word(word))
            && let Some(join_at) = words
                .iter()
                .position(|word| word.eq_ignore_ascii_case("Join"))
        {
            return join_words(&format!("{algorithm} Join"), &words[join_at + 1..]);
        }
    }
    // `Index Scan Backward using idx on t` and the bare form.
    let mut out = String::with_capacity(rest.len());
    let mut skipped = false;
    for (position, word) in words.iter().enumerate() {
        if !skipped && position > 0 && word.eq_ignore_ascii_case("Backward") {
            skipped = true;
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn join_words(head: &str, tail: &[&str]) -> String {
    if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head} {}", tail.join(" "))
    }
}

/// `SEARCH c USING INTEGER PRIMARY KEY (rowid=?)` -> `SEARCH USING INTEGER
/// PRIMARY KEY`; `SCAN TABLE o` / `SCAN o` -> `SCAN`; lines that do not name
/// a table (`SCAN CONSTANT ROW`, `USE TEMP B-TREE FOR ORDER BY`) come back
/// unchanged.
fn canonical_sqlite(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let Some(verb) = words.first() else {
        return String::new();
    };
    if !verb.eq_ignore_ascii_case("SCAN") && !verb.eq_ignore_ascii_case("SEARCH") {
        return text.trim().to_string();
    }
    let verb = verb.to_ascii_uppercase();
    let Some(second) = words.get(1) else {
        return verb;
    };
    if second.chars().all(|ch| ch.is_ascii_digit())
        && words
            .get(2)
            .is_some_and(|word| word.eq_ignore_ascii_case("CONSTANT"))
    {
        return format!("{verb} CONSTANT ROWS");
    }
    for keyword in ["CONSTANT", "SUBQUERY", "VIRTUAL"] {
        if second.eq_ignore_ascii_case(keyword) {
            return match keyword {
                "CONSTANT" => format!("{verb} CONSTANT ROW"),
                "SUBQUERY" => format!("{verb} SUBQUERY"),
                _ => format!("{verb} VIRTUAL TABLE INDEX"),
            };
        }
    }
    let Some(using_at) = words
        .iter()
        .position(|word| word.eq_ignore_ascii_case("USING"))
    else {
        if words
            .iter()
            .any(|word| word.eq_ignore_ascii_case("VIRTUAL"))
        {
            return format!("{verb} VIRTUAL TABLE INDEX");
        }
        return verb;
    };
    let mut out = format!("{verb} USING");
    for word in &words[using_at + 1..] {
        let upper = word.to_ascii_uppercase();
        if matches!(
            upper.as_str(),
            "INTEGER" | "PRIMARY" | "KEY" | "COVERING" | "INDEX" | "AUTOMATIC" | "PARTIAL"
        ) {
            out.push(' ');
            out.push_str(&upper);
        } else {
            break;
        }
    }
    out
}

/// Category for an operator the glossary does not know, from words in its
/// name. Deliberately conservative: anything unclear is `Other`.
fn guess_category(name: &str) -> OperatorCategory {
    let lowered = name.to_ascii_lowercase();
    let has = |needle: &str| lowered.contains(needle);
    if has("nested loop") || has("nested_loop") {
        OperatorCategory::JoinNestedLoop
    } else if has("hash join") || has("hash_join") {
        OperatorCategory::JoinHash
    } else if has("merge join") || has("merge_join") {
        OperatorCategory::JoinMerge
    } else if has("join") {
        OperatorCategory::Join
    } else if has("exchange")
        || has("remotesource")
        || has("remote source")
        || has("remotemerge")
        || has("localmerge")
    {
        // Trino and Presto name the operators that move rows between
        // stages and drivers this way; nothing leaves the cluster.
        OperatorCategory::Parallelism
    } else if has("lookup") {
        OperatorCategory::Lookup
    } else if has("seek") || has("search") {
        OperatorCategory::Seek
    } else if has("scan") || has("read") {
        OperatorCategory::Scan
    } else if has("sort") || has("order") {
        OperatorCategory::Sort
    } else if has("window") {
        OperatorCategory::Window
    } else if has("aggregat") || has("group") || has("distinct") {
        OperatorCategory::Aggregate
    } else if has("remote") {
        OperatorCategory::Remote
    } else if has("insert") || has("update") || has("delete") || lowered == "merge" {
        OperatorCategory::Modify
    } else if has("union") || has("intersect") || has("except") || has("append") {
        OperatorCategory::SetOp
    } else if has("limit") || has("top") || has("offset") {
        OperatorCategory::Limit
    } else if has("filter") {
        OperatorCategory::Filter
    } else if has("spool") || has("materializ") || has("cache") || has("temp") {
        OperatorCategory::Spool
    } else if has("gather") || has("exchange") || has("parallel") {
        OperatorCategory::Parallelism
    } else if has("subquery") || has("cte") {
        OperatorCategory::Subquery
    } else if has("project") || has("expression") || has("compute") {
        OperatorCategory::Compute
    } else {
        OperatorCategory::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn key(dialect: PlanDialect, physical: &str, logical: &str) -> &'static str {
        operator_lookup(dialect, physical, logical)
            .map(|entry| entry.key)
            .unwrap_or("?")
    }

    #[test]
    fn keys_ids_and_names_are_well_formed() {
        let mut keys = HashSet::new();
        for (engine, entries) in all_operator_glossaries() {
            assert!(entries.len() > 10, "{engine} glossary is too small");
            let mut names = HashSet::new();
            for entry in entries {
                assert!(keys.insert(entry.key), "duplicate key {}", entry.key);
                let (prefix, id) = entry.key.split_once('.').expect("engine.id");
                assert_eq!(prefix, engine);
                assert!(
                    id.starts_with(|ch: char| ch.is_ascii_lowercase())
                        && id
                            .chars()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
                        && !id.ends_with('_')
                        && !id.contains("__"),
                    "bad id {}",
                    entry.key
                );
                assert_ne!(id, "unknown");
                assert!(!entry.names.is_empty());
                for name in entry.names {
                    assert!(
                        names.insert(name.to_ascii_lowercase()),
                        "{engine}: name {name} is listed twice"
                    );
                }
                for text in [entry.description, entry.performance] {
                    assert!(!text.contains(['{', '}']), "{}: braces", entry.key);
                    assert!(
                        !text.contains(['\u{2014}', '\u{2013}']),
                        "{}: dash",
                        entry.key
                    );
                    assert!(text.ends_with('.'), "{}: no full stop: {text}", entry.key);
                }
                assert!(
                    entry.description.chars().count() <= 200,
                    "{}: description is {} chars",
                    entry.key,
                    entry.description.chars().count()
                );
                assert!(
                    entry.performance.chars().count() <= 300,
                    "{}: performance is {} chars",
                    entry.key,
                    entry.performance.chars().count()
                );
            }
        }
    }

    #[test]
    fn operator_entry_finds_keys() {
        assert_eq!(
            operator_entry("postgres.seq_scan").map(|entry| entry.category),
            Some(OperatorCategory::Scan)
        );
        assert!(operator_entry("postgres.nope").is_none());
    }

    #[test]
    fn category_keys_round_trip() {
        for category in OperatorCategory::ALL {
            assert_eq!(OperatorCategory::from_key(category.key()), Some(category));
        }
    }

    #[test]
    fn sql_server_uses_the_logical_operator_when_it_changes_the_meaning() {
        let d = PlanDialect::SqlServer;
        assert_eq!(key(d, "Hash Match", "Inner Join"), "sqlserver.hash_match");
        assert_eq!(
            key(d, "Hash Match", "Aggregate"),
            "sqlserver.hash_match_aggregate"
        );
        assert_eq!(
            operator_lookup(d, "Parallelism", "Gather Streams").map(|entry| entry.category),
            Some(OperatorCategory::Parallelism)
        );
        assert_ne!(
            key(d, "Parallelism", "Gather Streams"),
            key(d, "Parallelism", "Repartition Streams")
        );
        assert_eq!(
            key(d, "Clustered Index Scan", "Clustered Index Scan"),
            "sqlserver.clustered_index_scan"
        );
        assert_eq!(
            operator_lookup(d, "Nested Loops", "Left Outer Join").map(|entry| entry.category),
            Some(OperatorCategory::JoinNestedLoop)
        );
    }

    #[test]
    fn sql_server_key_lookup_is_recognised_from_the_node() {
        let node = PlanNode {
            physical_op: "Clustered Index Seek".to_string(),
            logical_op: "Clustered Index Seek".to_string(),
            warnings: vec![PlanWarning::KeyLookup {
                table: "Orders".to_string(),
                index: "PK_Orders".to_string(),
            }],
            ..Default::default()
        };
        let info = operator_info(PlanDialect::SqlServer, &node);
        assert_eq!(info.key, "sqlserver.key_lookup");
        assert_eq!(info.category, OperatorCategory::Lookup);
    }

    #[test]
    fn postgres_decorations_are_stripped() {
        let d = PlanDialect::Postgres;
        assert_eq!(key(d, "Parallel Seq Scan", ""), "postgres.seq_scan");
        assert_eq!(key(d, "Seq Scan on orders o", ""), "postgres.seq_scan");
        assert_eq!(
            key(d, "Index Scan Backward using i on t", ""),
            "postgres.index_scan"
        );
        assert_eq!(
            key(d, "Index Only Scan using i on t", ""),
            "postgres.index_only_scan"
        );
        assert_eq!(key(d, "Hash Left Join", ""), key(d, "Hash Join", ""));
        assert_eq!(
            key(d, "Nested Loop Semi Join", ""),
            key(d, "Nested Loop", "")
        );
        assert_eq!(
            operator_lookup(d, "Hash", "").map(|entry| entry.category),
            Some(OperatorCategory::Spool)
        );
        assert_eq!(
            key(d, "Finalize GroupAggregate", ""),
            key(d, "GroupAggregate", "")
        );
        assert_eq!(
            key(d, "Partial HashAggregate", ""),
            key(d, "HashAggregate", "")
        );
        assert_ne!(
            key(d, "Bitmap Heap Scan", ""),
            key(d, "Bitmap Index Scan", "")
        );
    }

    #[test]
    fn postgres_json_aggregate_uses_the_strategy() {
        let node = PlanNode {
            physical_op: "Aggregate".to_string(),
            logical_op: "Aggregate".to_string(),
            properties: vec![crate::plans::model::PlanProperty {
                name: "Strategy".to_string(),
                value: "Hashed".to_string(),
            }],
            ..Default::default()
        };
        let info = operator_info(PlanDialect::Postgres, &node);
        assert_eq!(
            Some(info.key),
            operator_lookup(PlanDialect::Postgres, "HashAggregate", "").map(|entry| entry.key)
        );
    }

    #[test]
    fn mysql_tree_phrases_and_access_types() {
        let d = PlanDialect::MySql;
        assert_eq!(
            key(d, "Table scan on o", ""),
            key(d, "Table: o", "Access: ALL")
        );
        assert_eq!(
            operator_lookup(d, "Table scan on <temporary>", "").map(|entry| entry.category),
            Some(OperatorCategory::Scan)
        );
        assert_eq!(
            operator_lookup(
                d,
                "Single-row index lookup on c using PRIMARY (id=o.customer_id)",
                ""
            )
            .map(|entry| entry.category),
            Some(OperatorCategory::Seek)
        );
        assert_ne!(
            key(d, "Single-row index lookup on c using PRIMARY", ""),
            key(d, "Index lookup on c using k", "")
        );
        assert_eq!(
            operator_lookup(d, "Filter: (o.total > 1200.00)", "").map(|entry| entry.category),
            Some(OperatorCategory::Filter)
        );
        assert_eq!(
            operator_lookup(d, "Sort: revenue DESC", "").map(|entry| entry.category),
            Some(OperatorCategory::Sort)
        );
        assert_eq!(
            operator_lookup(d, "Nested loop inner join", "").map(|entry| entry.category),
            Some(OperatorCategory::JoinNestedLoop)
        );
        assert_ne!(key(d, "Query Block", ""), "?");
        assert_ne!(
            key(d, "Select #2 (subquery in condition; dependent)", ""),
            "?"
        );
        assert_eq!(
            key(PlanDialect::MariaDb, "Table: c", "Access: eq_ref"),
            key(d, "Single-row index lookup on c using PRIMARY", "")
        );
    }

    #[test]
    fn oracle_matches_the_longest_operation() {
        let d = PlanDialect::Oracle;
        assert_eq!(
            key(d, "TABLE ACCESS FULL ORDERS", "TABLE ACCESS"),
            "oracle.table_access_full"
        );
        assert_ne!(
            key(d, "TABLE ACCESS BY INDEX ROWID BATCHED ORDERS", ""),
            key(d, "TABLE ACCESS FULL", "")
        );
        assert_ne!(
            key(d, "INDEX RANGE SCAN IX_O", ""),
            key(d, "INDEX FULL SCAN", "")
        );
        assert_eq!(
            operator_lookup(d, "HASH JOIN RIGHT SEMI", "").map(|entry| entry.category),
            Some(OperatorCategory::JoinHash)
        );
        assert_eq!(
            operator_lookup(d, "SORT ORDER BY", "").map(|entry| entry.category),
            Some(OperatorCategory::Sort)
        );
        assert_eq!(
            operator_lookup(d, "HASH GROUP BY", "").map(|entry| entry.category),
            Some(OperatorCategory::Aggregate)
        );
        assert_eq!(
            operator_lookup(d, "SELECT STATEMENT", "").map(|entry| entry.category),
            Some(OperatorCategory::Result)
        );
    }

    #[test]
    fn sqlite_lines_are_canonicalized() {
        let d = PlanDialect::Sqlite;
        assert_eq!(canonical_sqlite("SCAN o"), "SCAN");
        assert_eq!(canonical_sqlite("SCAN TABLE orders AS o"), "SCAN");
        assert_eq!(
            canonical_sqlite("SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
            "SEARCH USING INTEGER PRIMARY KEY"
        );
        assert_eq!(
            canonical_sqlite("SEARCH TABLE o USING COVERING INDEX ix_o (a=? AND b>?)"),
            "SEARCH USING COVERING INDEX"
        );
        assert_eq!(
            canonical_sqlite("SEARCH o USING AUTOMATIC PARTIAL COVERING INDEX (a=?)"),
            "SEARCH USING AUTOMATIC PARTIAL COVERING INDEX"
        );
        assert_eq!(canonical_sqlite("SCAN CONSTANT ROW"), "SCAN CONSTANT ROW");
        assert_eq!(
            canonical_sqlite("SCAN 3 CONSTANT ROWS"),
            "SCAN CONSTANT ROWS"
        );
        assert_ne!(key(d, "SCAN 3 CONSTANT ROWS", ""), key(d, "SCAN o", ""));
        assert_eq!(canonical_sqlite("SCAN t EXISTS"), "SCAN");
        assert_eq!(canonical_sqlite("SCAN SUBQUERY 2 AS x"), "SCAN SUBQUERY");
        assert_eq!(
            operator_lookup(d, "SCAN o", "").map(|entry| entry.category),
            Some(OperatorCategory::Scan)
        );
        assert_eq!(
            operator_lookup(d, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)", "")
                .map(|entry| entry.category),
            Some(OperatorCategory::Seek)
        );
        assert_ne!(
            key(d, "SCAN o USING COVERING INDEX ix", ""),
            key(d, "SCAN o", "")
        );
        assert_eq!(
            operator_lookup(d, "USE TEMP B-TREE FOR ORDER BY", "").map(|entry| entry.category),
            Some(OperatorCategory::Sort)
        );
        assert_ne!(key(d, "LIST SUBQUERY 1", ""), "?");
        assert_ne!(key(d, "CREATE BLOOM FILTER", ""), "?");
    }

    #[test]
    fn duckdb_and_clickhouse_match_with_object_names_attached() {
        assert_eq!(
            operator_lookup(PlanDialect::DuckDb, "TABLE_SCAN memory.main.orders", "")
                .map(|entry| entry.category),
            Some(OperatorCategory::Scan)
        );
        assert_eq!(
            operator_lookup(PlanDialect::DuckDb, "HASH_GROUP_BY", "").map(|entry| entry.category),
            Some(OperatorCategory::Aggregate)
        );
        assert_ne!(key(PlanDialect::DuckDb, "QUERY", ""), "?");
        assert_eq!(
            operator_lookup(
                PlanDialect::ClickHouse,
                "Expression (Project names + Projection)",
                ""
            )
            .map(|entry| entry.category),
            Some(OperatorCategory::Compute)
        );
        assert_eq!(
            operator_lookup(
                PlanDialect::ClickHouse,
                "ReadFromMergeTree default.orders",
                ""
            )
            .map(|entry| entry.category),
            Some(OperatorCategory::Scan)
        );
    }

    #[test]
    fn unknown_operators_get_a_guessed_category() {
        let node = PlanNode {
            physical_op: "Quantum Flux Scan".to_string(),
            ..Default::default()
        };
        let info = operator_info(PlanDialect::Postgres, &node);
        assert!(!info.known);
        assert_eq!(info.key, "postgres.unknown");
        assert_eq!(info.category, OperatorCategory::Scan);
        assert_eq!(info.name, "Quantum Flux Scan");
    }

    /// Every operator of every sample plan the online viewer ships, and of
    /// every golden fixture, has a glossary entry for its own engine.
    #[test]
    fn every_sample_operator_is_in_the_glossary() {
        use crate::plans::analysis::plan_engine;
        use crate::plans::import::import_plan_document;
        let root = env!("CARGO_MANIFEST_DIR");
        let mut dirs = vec![format!("{root}/../../apps/planview-wasm/samples")];
        for engine in ["cockroachdb", "trino", "spark"] {
            dirs.push(format!("{root}/tests/golden/plans/{engine}"));
        }
        let mut plans = 0;
        for dir in dirs {
            let mut files: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| matches!(ext, "txt" | "json" | "xml"))
                })
                .collect();
            files.sort();
            for file in files {
                let text = std::fs::read_to_string(&file).unwrap();
                let Ok(imported) = import_plan_document(&text) else {
                    continue;
                };
                for plan in &imported.plans {
                    plans += 1;
                    let engine = plan_engine(plan);
                    for node in &plan.nodes {
                        let info = operator_info_for(engine, node);
                        assert!(
                            info.known,
                            "{}: `{}` / `{}` is not in the {} glossary",
                            file.display(),
                            node.physical_op,
                            node.logical_op,
                            engine.name()
                        );
                    }
                }
            }
        }
        assert!(plans >= 40, "only {plans} sample plans were checked");
    }
}
