//! "Learn more" links from plan findings and plan operators to pages on
//! sqlly.app.
//!
//! The table is one JSON file, `learn_links.json`, embedded here and read by
//! the site's own tests too (`server/Sqlly.Tests/PlanLearnLinksTests.cs`
//! checks that every path and fragment is a real page). Both plan viewers,
//! the desktop one and the online tool, read the table through this module so
//! a finding links to the same article everywhere.
//!
//! Keys are plain strings: a finding kind id (`key_lookup`), an operator
//! category key (`join_hash`) or a glossary operator key
//! (`sqlserver.key_lookup`). A key either has links or an explicit reason for
//! having none, and the tests below hold the table to that for every finding
//! kind, every category and every operator in the glossary.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

use super::dialects::PlanDialect;
use super::glossary::{OperatorCategory, operator_entry};
use super::import::PlanEngine;

/// The embedded table, as shipped. Exposed so a host that cannot call Rust
/// (a web page, a test in another language) can be handed the same bytes.
pub const LEARN_LINKS_JSON: &str = include_str!("learn_links.json");

/// The site every path in the table is relative to.
pub const LEARN_SITE_ORIGIN: &str = "https://sqlly.app";

/// What kind of page a link opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearnLinkType {
    /// A guide or syntax page that explains the subject.
    Article,
    /// A reference page for one SQL function.
    Function,
    /// A reference page for one server setting.
    Setting,
    /// A ready-made diagnostic query.
    Snippet,
    /// An interactive tool on the site.
    Tool,
}

impl LearnLinkType {
    /// Stable id, the same string the JSON uses.
    pub fn as_str(self) -> &'static str {
        match self {
            LearnLinkType::Article => "article",
            LearnLinkType::Function => "function",
            LearnLinkType::Setting => "setting",
            LearnLinkType::Snippet => "snippet",
            LearnLinkType::Tool => "tool",
        }
    }
}

/// One link to a page on the site.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LearnLink {
    /// Site-relative path, optionally with a `#fragment`
    /// (`/sql-performance/key-lookup`).
    pub path: String,
    /// Short link text.
    pub title: String,
    /// One line saying what the reader will learn.
    pub learn: String,
    /// What kind of page it is.
    #[serde(rename = "type")]
    pub link_type: LearnLinkType,
    /// Engines the link applies to, as [`learn_engine_key`] ids. Empty means
    /// every engine.
    #[serde(default)]
    pub engines: Vec<String>,
}

impl LearnLink {
    /// True when the link is written for particular engines.
    pub fn is_engine_specific(&self) -> bool {
        !self.engines.is_empty()
    }

    /// True when the link applies to `dialect`.
    pub fn applies_to(&self, dialect: PlanDialect) -> bool {
        self.applies_to_key(learn_engine_key(dialect))
    }

    /// True when the link applies to `engine`.
    pub fn applies_to_engine(&self, engine: PlanEngine) -> bool {
        self.applies_to_key(learn_engine_key_for(engine))
    }

    fn applies_to_key(&self, engine: &str) -> bool {
        self.engines.is_empty() || self.engines.iter().any(|e| e == engine)
    }

    /// Absolute URL for this link. See [`learn_url`].
    pub fn url(&self, source: LearnSource, campaign: &str) -> String {
        learn_url(&self.path, source, campaign)
    }
}

/// Where a link is being opened from. It becomes the `utm_source` of the URL
/// so the site's referral log can tell the two viewers apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LearnSource {
    /// The desktop app's plan view.
    DesktopApp,
    /// The online plan viewer at `/tools/query-plan-viewer`.
    WebTool,
}

impl LearnSource {
    /// The `utm_source` value.
    pub fn as_str(self) -> &'static str {
        match self {
            LearnSource::DesktopApp => "sqlly-app",
            LearnSource::WebTool => "sqlly-web",
        }
    }
}

/// One key's entry: its links, or the reason it has none.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnEntry {
    /// Links in the order the file lists them.
    #[serde(default)]
    pub links: Vec<LearnLink>,
    /// Why the key deliberately has no link.
    #[serde(default)]
    pub none: Option<String>,
}

/// The whole table.
#[derive(Debug, Clone, Deserialize)]
pub struct LearnTable {
    /// Format version of the file.
    pub version: u32,
    /// Finding kind id -> entry.
    pub findings: HashMap<String, LearnEntry>,
    /// Operator category key -> entry.
    pub categories: HashMap<String, LearnEntry>,
    /// Glossary operator key -> entry, for the operators that have a page of
    /// their own beyond their category's.
    pub operators: HashMap<String, LearnEntry>,
}

/// The parsed table. The file is part of the build, so it parsing is checked
/// by this module's tests; a broken file yields an empty table, never a panic.
pub fn learn_table() -> &'static LearnTable {
    static TABLE: LazyLock<LearnTable> = LazyLock::new(|| {
        serde_json::from_str(LEARN_LINKS_JSON).unwrap_or_else(|_| LearnTable {
            version: 0,
            findings: HashMap::new(),
            categories: HashMap::new(),
            operators: HashMap::new(),
        })
    });
    &TABLE
}

/// Engine id used in a link's `engines` list. The same ids as the operator
/// glossary, except that MariaDB is `mariadb` and not `mysql`: some MySQL
/// settings pages do not apply to MariaDB.
pub fn learn_engine_key(dialect: PlanDialect) -> &'static str {
    match dialect {
        PlanDialect::SqlServer => "sqlserver",
        PlanDialect::Postgres => "postgres",
        PlanDialect::MySql => "mysql",
        PlanDialect::MariaDb => "mariadb",
        PlanDialect::Oracle => "oracle",
        PlanDialect::Sqlite => "sqlite",
        PlanDialect::DuckDb => "duckdb",
        PlanDialect::ClickHouse => "clickhouse",
    }
}

/// [`learn_engine_key`] for any engine plans can be imported from:
/// CockroachDB, Trino and Spark are `cockroachdb`, `trino` and `spark`, so a
/// link written for PostgreSQL is not offered on their plans.
pub fn learn_engine_key_for(engine: PlanEngine) -> &'static str {
    match engine {
        PlanEngine::CockroachDb => "cockroachdb",
        PlanEngine::Trino => "trino",
        PlanEngine::Spark => "spark",
        other => learn_engine_key(other.analysis_dialect()),
    }
}

/// Links for a finding kind id (`FindingKind::as_str()`), filtered to
/// `dialect` and ordered for display: articles before settings, snippets,
/// functions and tools; within each of those two groups, links written for
/// this engine before general ones; otherwise the order of the file. An
/// unknown kind, or one marked as having no link, returns an empty list.
pub fn links_for_finding(kind: &str, dialect: PlanDialect) -> Vec<&'static LearnLink> {
    finding_links(kind, learn_engine_key(dialect))
}

/// [`links_for_finding`] for any engine plans can be imported from. Pass
/// `PlanAnalysis::engine`.
pub fn engine_links_for_finding(kind: &str, engine: PlanEngine) -> Vec<&'static LearnLink> {
    finding_links(kind, learn_engine_key_for(engine))
}

fn finding_links(kind: &str, engine: &str) -> Vec<&'static LearnLink> {
    ordered(
        learn_table()
            .findings
            .get(kind)
            .into_iter()
            .flat_map(|entry| entry.links.iter()),
        engine,
    )
}

/// Links for an operator, by glossary key (`postgres.bitmap_heap_scan`) or by
/// category key (`scan`). The operator's own links come first, then its
/// category's, each ordered as in [`links_for_finding`], with a page listed
/// once. An unrecognised operator (`postgres.unknown`) has no category here;
/// use [`links_for_operator_in`] with the category the plan node resolved to.
pub fn links_for_operator(key: &str, dialect: PlanDialect) -> Vec<&'static LearnLink> {
    let category =
        OperatorCategory::from_key(key).or_else(|| operator_entry(key).map(|entry| entry.category));
    operator_links(key, category, learn_engine_key(dialect))
}

/// [`links_for_operator`] for any engine plans can be imported from.
pub fn engine_links_for_operator(key: &str, engine: PlanEngine) -> Vec<&'static LearnLink> {
    let category =
        OperatorCategory::from_key(key).or_else(|| operator_entry(key).map(|entry| entry.category));
    operator_links(key, category, learn_engine_key_for(engine))
}

/// [`links_for_operator_in`] for any engine plans can be imported from. Pass
/// `OperatorInfo::key`, `OperatorInfo::category` and `PlanAnalysis::engine`.
pub fn engine_links_for_operator_in(
    key: &str,
    category: OperatorCategory,
    engine: PlanEngine,
) -> Vec<&'static LearnLink> {
    operator_links(key, Some(category), learn_engine_key_for(engine))
}

/// [`links_for_operator`] with the category given by the caller, for
/// `OperatorInfo` values whose key is not in the glossary.
pub fn links_for_operator_in(
    key: &str,
    category: OperatorCategory,
    dialect: PlanDialect,
) -> Vec<&'static LearnLink> {
    operator_links(key, Some(category), learn_engine_key(dialect))
}

/// The reason a finding kind deliberately has no link, when it has one.
pub fn finding_without_link(kind: &str) -> Option<&'static str> {
    learn_table().findings.get(kind)?.none.as_deref()
}

/// Absolute URL for a site-relative `path` from the table, tagged so the
/// site can attribute the visit:
/// `https://sqlly.app/sql-performance/key-lookup?utm_source=sqlly-app&utm_medium=plan-viewer&utm_campaign=key_lookup`.
/// `campaign` is the finding kind or operator key the link was shown for; it
/// is reduced to lowercase letters, digits, `.`, `-` and `_` (60 at most) and
/// left out when nothing remains. A `#fragment` on the path stays at the end.
pub fn learn_url(path: &str, source: LearnSource, campaign: &str) -> String {
    let (page, fragment) = match path.split_once('#') {
        Some((page, fragment)) => (page, Some(fragment)),
        None => (path, None),
    };
    let separator = if page.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{LEARN_SITE_ORIGIN}{}{page}{separator}utm_source={}&utm_medium=plan-viewer",
        if page.starts_with('/') { "" } else { "/" },
        source.as_str(),
    );
    let campaign: String = campaign
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        .take(60)
        .collect();
    if !campaign.is_empty() {
        url.push_str("&utm_campaign=");
        url.push_str(&campaign);
    }
    if let Some(fragment) = fragment.filter(|f| !f.is_empty()) {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

fn operator_links(
    key: &str,
    category: Option<OperatorCategory>,
    engine: &str,
) -> Vec<&'static LearnLink> {
    let table = learn_table();
    let mut links = ordered(
        table
            .operators
            .get(key)
            .into_iter()
            .flat_map(|entry| entry.links.iter()),
        engine,
    );
    let from_category = ordered(
        category
            .and_then(|category| table.categories.get(category.key()))
            .into_iter()
            .flat_map(|entry| entry.links.iter()),
        engine,
    );
    for link in from_category {
        if !links.iter().any(|seen| seen.path == link.path) {
            links.push(link);
        }
    }
    links
}

/// Filter to `engine`, then a stable sort: articles first, and inside each
/// group the engine's own links before general ones.
fn ordered(
    links: impl Iterator<Item = &'static LearnLink>,
    engine: &str,
) -> Vec<&'static LearnLink> {
    let mut links: Vec<&'static LearnLink> =
        links.filter(|link| link.applies_to_key(engine)).collect();
    links.sort_by_key(|link| {
        (
            link.link_type != LearnLinkType::Article,
            !link.is_engine_specific(),
        )
    });
    links
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::findings::FindingKind;
    use super::super::glossary::all_operator_glossaries;
    use super::*;

    /// Every finding kind id, from the enum itself, so a kind added to
    /// `FindingKind` without a link decision fails the tests below.
    fn finding_kinds() -> Vec<&'static str> {
        FindingKind::ALL.iter().map(|kind| kind.as_str()).collect()
    }

    /// Every id a link's `engines` list may hold.
    fn engine_ids() -> Vec<&'static str> {
        PlanEngine::ALL
            .into_iter()
            .map(learn_engine_key_for)
            .collect()
    }

    const DIALECTS: [PlanDialect; 8] = [
        PlanDialect::SqlServer,
        PlanDialect::Postgres,
        PlanDialect::MySql,
        PlanDialect::MariaDb,
        PlanDialect::Sqlite,
        PlanDialect::DuckDb,
        PlanDialect::ClickHouse,
        PlanDialect::Oracle,
    ];

    fn parsed() -> LearnTable {
        serde_json::from_str(LEARN_LINKS_JSON).expect("learn_links.json parses")
    }

    fn sections(table: &LearnTable) -> [(&'static str, &HashMap<String, LearnEntry>); 3] {
        [
            ("findings", &table.findings),
            ("categories", &table.categories),
            ("operators", &table.operators),
        ]
    }

    #[test]
    fn json_parses_and_is_the_table_in_use() {
        let table = parsed();
        assert_eq!(table.version, 1);
        assert_eq!(learn_table().version, 1);
        assert_eq!(learn_table().findings.len(), table.findings.len());
    }

    #[test]
    fn every_entry_has_links_or_a_reason_and_never_both() {
        let table = parsed();
        for (section, entries) in sections(&table) {
            for (key, entry) in entries {
                let reason = entry.none.as_deref().map(str::trim).unwrap_or("");
                assert!(
                    entry.links.is_empty() != reason.is_empty(),
                    "{section}.{key} needs links or a none reason, not both and not neither"
                );
            }
        }
    }

    #[test]
    fn links_are_complete_and_well_formed() {
        let table = parsed();
        for (section, entries) in sections(&table) {
            for (key, entry) in entries {
                let mut paths = HashSet::new();
                for link in &entry.links {
                    let at = format!("{section}.{key} -> {}", link.path);
                    assert!(link.path.starts_with('/'), "{at}: path is site-relative");
                    assert!(!link.path.contains("://"), "{at}: no absolute URLs");
                    assert!(!link.path.contains('?'), "{at}: no query string");
                    assert!(!link.path.ends_with('#'), "{at}: empty fragment");
                    assert_eq!(link.path, link.path.trim(), "{at}: stray whitespace");
                    assert_eq!(
                        link.path,
                        link.path.to_ascii_lowercase(),
                        "{at}: lowercase path"
                    );
                    assert!(paths.insert(link.path.as_str()), "{at}: listed twice");
                    assert!(!link.title.trim().is_empty(), "{at}: empty title");
                    assert!(
                        link.title.len() <= 60,
                        "{at}: title is link text, keep it short"
                    );
                    assert!(!link.learn.trim().is_empty(), "{at}: empty learn line");
                    assert!(link.learn.ends_with('.'), "{at}: learn line is a sentence");
                    assert!(!link.learn.contains('\n'), "{at}: learn line is one line");
                    let mut engines = HashSet::new();
                    for engine in &link.engines {
                        assert!(
                            engine_ids().contains(&engine.as_str()),
                            "{at}: engine {engine}"
                        );
                        assert!(engines.insert(engine), "{at}: engine {engine} twice");
                    }
                }
            }
        }
    }

    #[test]
    fn one_path_has_one_title_and_one_type() {
        let table = parsed();
        let mut seen: HashMap<&str, (&str, LearnLinkType)> = HashMap::new();
        for (_, entries) in sections(&table) {
            for link in entries.values().flat_map(|entry| entry.links.iter()) {
                let known = seen
                    .entry(link.path.as_str())
                    .or_insert((link.title.as_str(), link.link_type));
                assert_eq!(known.1, link.link_type, "{}", link.path);
                // Category entries title the operators page after the category;
                // every other page keeps one title wherever it is linked.
                if !link
                    .path
                    .starts_with("/sql-performance/execution-plan-operators#")
                {
                    assert_eq!(known.0, link.title, "{}", link.path);
                }
            }
        }
    }

    #[test]
    fn every_finding_kind_resolves_or_is_marked_none() {
        let table = parsed();
        let kinds = finding_kinds();
        for kind in &kinds {
            assert!(
                table.findings.contains_key(*kind),
                "no entry for finding {kind}"
            );
        }
        let known: HashSet<&str> = kinds.iter().copied().collect();
        assert_eq!(known.len(), kinds.len());
        for key in table.findings.keys() {
            assert!(known.contains(key.as_str()), "{key} is not a finding kind");
        }
    }

    #[test]
    fn a_finding_with_links_has_a_general_one_or_covers_named_engines() {
        // A kind whose links are all engine-specific is fine (JIT is
        // PostgreSQL only); what must not happen is a kind with links that
        // resolves to nothing on every engine.
        let table = parsed();
        for (kind, entry) in &table.findings {
            if entry.links.is_empty() {
                continue;
            }
            let reachable = DIALECTS
                .iter()
                .any(|dialect| !links_for_finding(kind, *dialect).is_empty());
            assert!(reachable, "{kind} resolves to no link on any engine");
        }
    }

    #[test]
    fn every_category_resolves_on_every_engine() {
        let table = parsed();
        assert_eq!(table.categories.len(), OperatorCategory::ALL.len());
        for category in OperatorCategory::ALL {
            for dialect in DIALECTS {
                let links = links_for_operator(category.key(), dialect);
                assert!(!links.is_empty(), "{} on {dialect:?}", category.key());
                assert!(
                    links.iter().any(|link| link
                        .path
                        .starts_with("/sql-performance/execution-plan-operators#")),
                    "{} links its section of the operators page",
                    category.key()
                );
            }
        }
    }

    #[test]
    fn every_glossary_operator_resolves() {
        let engine_of = |engine: &str| match engine {
            "sqlserver" => PlanEngine::SqlServer,
            "postgres" => PlanEngine::Postgres,
            "mysql" => PlanEngine::MySql,
            "oracle" => PlanEngine::Oracle,
            "sqlite" => PlanEngine::Sqlite,
            "duckdb" => PlanEngine::DuckDb,
            "clickhouse" => PlanEngine::ClickHouse,
            "cockroachdb" => PlanEngine::CockroachDb,
            "trino" => PlanEngine::Trino,
            "spark" => PlanEngine::Spark,
            other => panic!("no engine for glossary {other}"),
        };
        let mut count = 0;
        for (engine, entries) in all_operator_glossaries() {
            for entry in entries {
                count += 1;
                let links = engine_links_for_operator(entry.key, engine_of(engine));
                assert!(!links.is_empty(), "{} has no link", entry.key);
            }
        }
        assert!(count > 300, "glossary looks truncated: {count} operators");
    }

    #[test]
    fn operator_entries_name_real_operators() {
        let table = parsed();
        for key in table.operators.keys() {
            assert!(
                operator_entry(key).is_some(),
                "{key} is not in the operator glossary"
            );
        }
    }

    #[test]
    fn operator_links_come_before_category_links() {
        let links = links_for_operator("sqlserver.key_lookup", PlanDialect::SqlServer);
        assert_eq!(links[0].path, "/sql-performance/key-lookup");
        assert!(
            links
                .iter()
                .any(|l| l.path == "/sql-performance/execution-plan-operators#lookup")
        );
        let paths: HashSet<&str> = links.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths.len(), links.len(), "a page is listed once");
    }

    #[test]
    fn unknown_operator_uses_the_given_category() {
        assert!(links_for_operator("postgres.unknown", PlanDialect::Postgres).is_empty());
        let links = links_for_operator_in(
            "postgres.unknown",
            OperatorCategory::Sort,
            PlanDialect::Postgres,
        );
        assert_eq!(
            links[0].path,
            "/sql-performance/execution-plan-operators#sort"
        );
    }

    #[test]
    fn links_are_filtered_and_ordered_by_engine() {
        let pg = links_for_finding("spill", PlanDialect::Postgres);
        assert_eq!(pg[0].path, "/sql-performance/sort-and-hash-spills");
        assert!(
            pg.iter()
                .any(|l| l.path == "/sql-configuration/postgresql/work-mem")
        );
        assert!(pg.iter().all(|l| l.applies_to(PlanDialect::Postgres)));
        assert!(
            !pg.iter()
                .any(|l| l.path.starts_with("/sql-configuration/mysql/"))
        );

        // Articles lead; among the rest the engine's own pages come first.
        let first_other = pg
            .iter()
            .position(|l| l.link_type != LearnLinkType::Article)
            .expect("spill has settings");
        assert!(
            pg[first_other..]
                .iter()
                .all(|l| l.link_type != LearnLinkType::Article)
        );

        // An engine-specific article outranks a general one.
        let serial = links_for_finding("serial_plan_forced", PlanDialect::SqlServer);
        assert!(serial[0].is_engine_specific());
        assert!(
            serial[0]
                .path
                .starts_with("/sql-performance/parallel-query-plans#")
        );
    }

    #[test]
    fn mariadb_is_not_mysql() {
        let mysql = links_for_finding("expensive_sort", PlanDialect::MySql);
        let maria = links_for_finding("expensive_sort", PlanDialect::MariaDb);
        assert!(
            mysql
                .iter()
                .any(|l| l.path == "/sql-snippets/sort-heavy-queries")
        );
        assert!(
            !maria
                .iter()
                .any(|l| l.path == "/sql-snippets/sort-heavy-queries")
        );
        assert!(
            maria
                .iter()
                .any(|l| l.path == "/sql-configuration/mysql/sort-buffer-size")
        );
    }

    #[test]
    fn unknown_and_unlinked_kinds_return_nothing() {
        assert!(links_for_finding("no_such_kind", PlanDialect::Postgres).is_empty());
        assert!(links_for_finding("schema_stale", PlanDialect::Postgres).is_empty());
        assert!(finding_without_link("schema_stale").is_some());
        assert!(finding_without_link("key_lookup").is_none());
        assert!(finding_without_link("no_such_kind").is_none());
    }

    #[test]
    fn urls_are_absolute_and_tagged() {
        assert_eq!(
            learn_url(
                "/sql-performance/key-lookup",
                LearnSource::DesktopApp,
                "key_lookup"
            ),
            "https://sqlly.app/sql-performance/key-lookup?utm_source=sqlly-app&utm_medium=plan-viewer&utm_campaign=key_lookup"
        );
        assert_eq!(
            learn_url(
                "/sql-performance/row-estimate-mismatch#table-variables",
                LearnSource::WebTool,
                "SqlServer.Table Scan!",
            ),
            "https://sqlly.app/sql-performance/row-estimate-mismatch?utm_source=sqlly-web&utm_medium=plan-viewer&utm_campaign=sqlserver.tablescan#table-variables"
        );
        assert_eq!(
            learn_url("/tools/join-visualizer", LearnSource::DesktopApp, ""),
            "https://sqlly.app/tools/join-visualizer?utm_source=sqlly-app&utm_medium=plan-viewer"
        );
        let link = &links_for_finding("key_lookup", PlanDialect::SqlServer)[0];
        assert!(
            link.url(LearnSource::DesktopApp, "key_lookup")
                .starts_with("https://sqlly.app/sql-performance/key-lookup?utm_source=sqlly-app")
        );
    }
}
