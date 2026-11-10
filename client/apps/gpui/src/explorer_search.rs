use std::collections::BTreeSet;
use std::ops::Range;

use sqlly_intel::intellisense::{MatchTier, object_match_tier};

use crate::schema_tree::NodeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectScope {
    Tables,
    Views,
    Routines,
    Columns,
    Sequences,
    Triggers,
    Synonyms,
    Extensions,
    Events,
    MaterializedViews,
    Constraints,
}

impl ObjectScope {
    pub const ALL: &'static [ObjectScope] = &[
        ObjectScope::Tables,
        ObjectScope::Views,
        ObjectScope::Routines,
        ObjectScope::Columns,
        ObjectScope::Sequences,
        ObjectScope::Triggers,
        ObjectScope::Synonyms,
        ObjectScope::Extensions,
        ObjectScope::Events,
        ObjectScope::MaterializedViews,
        ObjectScope::Constraints,
    ];

    pub fn label(self) -> String {
        crate::localization::text(match self {
            ObjectScope::Tables => "explorer.search.scope_tables",
            ObjectScope::Views => "explorer.search.scope_views",
            ObjectScope::Routines => "explorer.search.scope_routines",
            ObjectScope::Columns => "explorer.search.scope_columns",
            ObjectScope::Sequences => "explorer.search.scope_sequences",
            ObjectScope::Triggers => "explorer.search.scope_triggers",
            ObjectScope::Synonyms => "explorer.search.scope_synonyms",
            ObjectScope::Extensions => "explorer.search.scope_extensions",
            ObjectScope::Events => "explorer.search.scope_events",
            ObjectScope::MaterializedViews => "explorer.search.scope_materialized_views",
            ObjectScope::Constraints => "explorer.search.scope_constraints",
        })
    }

    fn matches_kind(self, kind: &NodeKind) -> bool {
        match self {
            ObjectScope::Tables => matches!(kind, NodeKind::Table { .. }),
            ObjectScope::Views => matches!(kind, NodeKind::View { .. }),
            ObjectScope::Routines => {
                matches!(kind, NodeKind::Procedure { .. } | NodeKind::Function { .. })
            }
            ObjectScope::Columns => {
                matches!(kind, NodeKind::Column | NodeKind::Parameter { .. })
            }
            ObjectScope::Sequences => matches!(kind, NodeKind::Sequence { .. }),
            ObjectScope::Triggers => matches!(kind, NodeKind::Trigger { .. }),
            ObjectScope::Synonyms => matches!(kind, NodeKind::Synonym { .. }),
            ObjectScope::Extensions => matches!(kind, NodeKind::Extension { .. }),
            ObjectScope::Events => matches!(kind, NodeKind::Event { .. }),
            ObjectScope::MaterializedViews => matches!(kind, NodeKind::MaterializedView { .. }),
            ObjectScope::Constraints => matches!(kind, NodeKind::Constraint { .. }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryMode {
    /// Tiered matcher (default).
    Ranked(String),
    /// `/pattern/` → case-insensitive regex; `/pattern/c` → case-sensitive.
    Regex(String, bool),
    /// User typed `/…` without a closing slash, or an invalid pattern.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerQuery {
    pub mode: QueryMode,
    /// Empty set = all kinds (chips are additive; none selected = no restriction).
    pub scopes: BTreeSet<ObjectScope>,
}

impl ExplorerQuery {
    pub fn parse(raw: &str, scopes: BTreeSet<ObjectScope>) -> Self {
        let trimmed = raw.trim();
        let mode = if trimmed.starts_with('/') {
            parse_regex_mode(trimmed)
        } else {
            QueryMode::Ranked(trimmed.to_string())
        };
        Self { mode, scopes }
    }

    pub fn is_empty(&self) -> bool {
        matches!(&self.mode, QueryMode::Ranked(s) if s.is_empty()) && self.scopes.is_empty()
    }

    /// Cache key: raw text + scopes, so `cached_flatten` can keep its (revision, key) cache.
    pub fn cache_key(&self) -> String {
        let mode = match &self.mode {
            QueryMode::Ranked(s) => format!("r:{s}"),
            QueryMode::Regex(p, c) => format!("x:{p}:{}", *c as u8),
            QueryMode::Invalid(s) => format!("i:{s}"),
        };
        let scopes: String = self
            .scopes
            .iter()
            .map(|s| format!("{:?}", s))
            .collect::<Vec<_>>()
            .join(",");
        format!("{mode}|{scopes}")
    }

    pub fn display_text(&self) -> String {
        match &self.mode {
            QueryMode::Ranked(s) => s.clone(),
            QueryMode::Regex(p, false) => format!("/{p}/"),
            QueryMode::Regex(p, true) => format!("/{p}/c"),
            QueryMode::Invalid(s) => s.clone(),
        }
    }

    pub fn kind_only(&self) -> bool {
        matches!(&self.mode, QueryMode::Ranked(s) if s.is_empty()) && !self.scopes.is_empty()
    }
}

fn parse_regex_mode(trimmed: &str) -> QueryMode {
    let rest = &trimmed[1..];
    let Some(close) = rest.find('/') else {
        return QueryMode::Invalid(trimmed.to_string());
    };
    let pattern = rest[..close].to_string();
    let flags = &rest[close + 1..];
    match flags {
        "" => QueryMode::Regex(pattern, false),
        "c" => QueryMode::Regex(pattern, true),
        _ => QueryMode::Invalid(trimmed.to_string()),
    }
}

enum CompiledMode {
    Ranked {
        needle: String,
        needle_lower: String,
    },
    Regex(regex::Regex),
    Invalid {
        message: String,
    },
    Empty,
    KindOnly,
}

/// One compiled query, built once per filter change, reused per node.
pub struct CompiledQuery {
    mode: CompiledMode,
    scopes: BTreeSet<ObjectScope>,
}

impl CompiledQuery {
    pub fn compile(q: &ExplorerQuery) -> Self {
        if q.is_empty() {
            return Self {
                mode: CompiledMode::Empty,
                scopes: q.scopes.clone(),
            };
        }
        if q.kind_only() {
            return Self {
                mode: CompiledMode::KindOnly,
                scopes: q.scopes.clone(),
            };
        }
        let mode = match &q.mode {
            QueryMode::Ranked(s) => CompiledMode::Ranked {
                needle: s.clone(),
                needle_lower: s.to_lowercase(),
            },
            QueryMode::Regex(pattern, case_sensitive) => {
                match regex::RegexBuilder::new(pattern)
                    .case_insensitive(!*case_sensitive)
                    .size_limit(1 << 20)
                    .build()
                {
                    Ok(re) => CompiledMode::Regex(re),
                    Err(e) => CompiledMode::Invalid {
                        message: crate::localization::format(
                            "explorer.search.invalid_regex",
                            &[("message", &e.to_string())],
                        ),
                    },
                }
            }
            QueryMode::Invalid(_) => CompiledMode::Invalid {
                message: crate::localization::text("explorer.search.unterminated_regex"),
            },
        };
        Self {
            mode,
            scopes: q.scopes.clone(),
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match &self.mode {
            CompiledMode::Invalid { message } => Some(message.as_str()),
            _ => None,
        }
    }

    /// `Some(tier)` when the title matches. Regex matches report `MatchTier::Substring`.
    pub fn match_title(&self, title: &str, title_lower: &str) -> Option<MatchTier> {
        match &self.mode {
            CompiledMode::Empty | CompiledMode::KindOnly | CompiledMode::Invalid { .. } => None,
            CompiledMode::Ranked { needle, .. } => object_match_tier(needle, title),
            CompiledMode::Regex(re) => {
                let _ = title_lower;
                re.is_match(title).then_some(MatchTier::Substring)
            }
        }
    }

    /// Byte range of the highlighted span in `title`, for `highlighted_title`.
    pub fn highlight_range(&self, title: &str, title_lower: &str) -> Option<Range<usize>> {
        match &self.mode {
            CompiledMode::Ranked { needle_lower, .. } => {
                if needle_lower.is_empty() || title_lower.len() != title.len() {
                    return None;
                }
                title_lower.find(needle_lower.as_str()).map(|start| {
                    let end = start + needle_lower.len();
                    start..end
                })
            }
            CompiledMode::Regex(re) => re.find(title).map(|m| m.start()..m.end()),
            CompiledMode::Empty | CompiledMode::KindOnly | CompiledMode::Invalid { .. } => None,
        }
    }

    pub fn allows_kind(&self, kind: &NodeKind) -> bool {
        if self.scopes.is_empty() {
            return true;
        }
        if is_structural_kind(kind) {
            return true;
        }
        self.scopes.iter().any(|s| s.matches_kind(kind))
    }

    pub fn is_scoped_object(&self, kind: &NodeKind) -> bool {
        if self.scopes.is_empty() {
            return is_rankable_object(kind);
        }
        self.scopes.iter().any(|s| s.matches_kind(kind))
    }

    pub fn kind_only(&self) -> bool {
        matches!(self.mode, CompiledMode::KindOnly)
    }
}

pub(crate) fn is_structural_kind(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Server { .. }
            | NodeKind::Group { .. }
            | NodeKind::Database(_)
            | NodeKind::SystemDatabasesFolder
            | NodeKind::SystemSchemasFolder
            | NodeKind::EmptySchemasFolder
            | NodeKind::SchemaFolder { .. }
            | NodeKind::TablesFolder
            | NodeKind::ViewsFolder
            | NodeKind::ProgrammabilityFolder
            | NodeKind::IndexesFolder { .. }
            | NodeKind::StatisticsFolder { .. }
            | NodeKind::ForeignKeysFolder { .. }
            | NodeKind::TypesFolder
            | NodeKind::TriggersFolder { .. }
            | NodeKind::ConstraintsFolder { .. }
            | NodeKind::DatabaseTriggersFolder
            | NodeKind::SequencesFolder
            | NodeKind::SynonymsFolder
            | NodeKind::ExtensionsFolder
            | NodeKind::EventsFolder
            | NodeKind::MaterializedViewsFolder
            | NodeKind::PinnedFolder { .. }
            | NodeKind::Error
            | NodeKind::AzureAccount
            | NodeKind::AzureSubscription
            | NodeKind::AzureSqlServer
            | NodeKind::AzureDatabase(_)
    )
}

pub(crate) fn is_rankable_object(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Table { .. }
            | NodeKind::View { .. }
            | NodeKind::Procedure { .. }
            | NodeKind::Function { .. }
            | NodeKind::Column
            | NodeKind::Parameter { .. }
            | NodeKind::Index { .. }
            | NodeKind::Statistic { .. }
            | NodeKind::ForeignKey { .. }
            | NodeKind::Type { .. }
            | NodeKind::Trigger { .. }
            | NodeKind::Constraint { .. }
            | NodeKind::Sequence { .. }
            | NodeKind::Synonym { .. }
            | NodeKind::Extension { .. }
            | NodeKind::Event { .. }
            | NodeKind::MaterializedView { .. }
    )
}

pub(crate) fn is_reveal_all_kind(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Table { .. }
            | NodeKind::View { .. }
            | NodeKind::Procedure { .. }
            | NodeKind::Function { .. }
            | NodeKind::MaterializedView { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_is_ranked() {
        let q = ExplorerQuery::parse("  Acc  ", BTreeSet::new());
        assert_eq!(q.mode, QueryMode::Ranked("Acc".to_string()));
        assert!(!q.is_empty());
        assert!(ExplorerQuery::parse("  ", BTreeSet::new()).is_empty());
    }

    #[test]
    fn parse_slash_pattern_is_regex_case_insensitive() {
        let q = ExplorerQuery::parse("/^acc/", BTreeSet::new());
        assert_eq!(q.mode, QueryMode::Regex("^acc".to_string(), false));
    }

    #[test]
    fn parse_slash_pattern_c_flag_is_case_sensitive() {
        let q = ExplorerQuery::parse("/Acc/c", BTreeSet::new());
        assert_eq!(q.mode, QueryMode::Regex("Acc".to_string(), true));
    }

    #[test]
    fn parse_unterminated_slash_is_invalid() {
        let q = ExplorerQuery::parse("/acc", BTreeSet::new());
        assert!(matches!(q.mode, QueryMode::Invalid(_)));
    }

    #[test]
    fn regex_invalid_pattern_matches_nothing() {
        let q = ExplorerQuery::parse("/[a/", BTreeSet::new());
        let compiled = CompiledQuery::compile(&q);
        assert!(compiled.match_title("Account", "account").is_none());
        assert!(compiled.error_message().is_some());
    }

    #[test]
    fn scopes_restrict_object_kinds_but_not_folders() {
        let mut scopes = BTreeSet::new();
        scopes.insert(ObjectScope::Tables);
        let compiled = CompiledQuery::compile(&ExplorerQuery::parse("acc", scopes));
        let table = NodeKind::Table {
            schema: "dbo".into(),
            name: "Account".into(),
        };
        let view = NodeKind::View {
            schema: "dbo".into(),
            name: "AccountView".into(),
        };
        assert!(compiled.allows_kind(&table));
        assert!(!compiled.allows_kind(&view));
        assert!(compiled.allows_kind(&NodeKind::TablesFolder));
        assert!(compiled.allows_kind(&NodeKind::Server {
            id: uuid::Uuid::nil()
        }));
    }

    #[test]
    fn highlight_range_for_regex_is_first_match() {
        let q = ExplorerQuery::parse("/acc/", BTreeSet::new());
        let compiled = CompiledQuery::compile(&q);
        assert_eq!(
            compiled.highlight_range("AccountHistory", "accounthistory"),
            Some(0..3)
        );
    }
}
