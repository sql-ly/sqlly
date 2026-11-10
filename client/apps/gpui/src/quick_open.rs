//! Quick-open object palette model: flat candidate list built from the loaded
//! explorer tree, fuzzy-ranked with the command palette's scorer. Pure logic —
//! no gpui entities — so it is unit-testable and wasm-clean.

use uuid::Uuid;

use crate::command_palette::CommandPaletteMatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickOpenKind {
    Server,
    Database,
    Table,
    View,
    Procedure,
    Function,
}

impl QuickOpenKind {
    /// Short label for the palette's left column (mirrors CommandCategory::label).
    pub fn label(self) -> String {
        let key = match self {
            Self::Server => "command_palette.objects.kind_server",
            Self::Database => "command_palette.objects.kind_database",
            Self::Table => "command_palette.objects.kind_table",
            Self::View => "command_palette.objects.kind_view",
            Self::Procedure => "command_palette.objects.kind_procedure",
            Self::Function => "command_palette.objects.kind_function",
        };
        crate::localization::text(key)
    }

    /// Ranking tiebreak: objects a user jumps to most (tables/views) win ties.
    fn rank_weight(self) -> i32 {
        match self {
            Self::Table | Self::View => 3,
            Self::Procedure | Self::Function => 2,
            Self::Database => 1,
            Self::Server => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickOpenCandidate {
    /// Exact explorer node id (`srv:{uuid}:db:{db}:tables:…`) — used for reveal.
    pub node_id: String,
    pub kind: QuickOpenKind,
    pub server_id: Option<Uuid>,
    /// Connection display name; shown as the trailing dim hint.
    pub server_name: String,
    pub database: Option<String>,
    pub schema: Option<String>,
    /// Bare object name (`Orders`), or database/server name for those kinds.
    pub name: String,
    /// What is matched and displayed: `dbo.Orders`, `master`, `Prod (relay)`.
    pub title: String,
    /// Lowercased `title`, copied from the tree so ranking does not re-allocate.
    pub title_lower: String,
}

#[derive(Debug, Clone)]
pub struct QuickOpenMatch {
    pub candidate: QuickOpenCandidate,
    pub score: i32,
}

/// Hard cap on rows handed to the renderer; the list is not virtualized.
pub const MAX_RESULTS: usize = 50;

/// Rank `candidates` for `query`. Empty query → first MAX_RESULTS in tree order.
/// Scoring: title via subsequence_score; also the bare `name`. If the query
/// contains a '.', also try `schema.name`. If no title/name hit, try `database`
/// and `server_name` with a -40 penalty (same idea as keyword matching in the
/// command palette). Add kind.rank_weight() and, when `active_server` matches,
/// +5 so the current connection's objects float above identical names elsewhere.
/// Stable sort by score desc, then title asc, then node_id asc. Truncate to
/// MAX_RESULTS.
pub fn rank(
    query: &str,
    candidates: &[QuickOpenCandidate],
    active_server: Option<Uuid>,
) -> Vec<QuickOpenMatch> {
    if query.is_empty() {
        return candidates
            .iter()
            .take(MAX_RESULTS)
            .map(|candidate| QuickOpenMatch {
                candidate: candidate.clone(),
                score: 0,
            })
            .collect();
    }

    let query_lower = query.to_lowercase();
    let mut matches: Vec<QuickOpenMatch> = Vec::new();

    for candidate in candidates {
        let Some(score) = score_candidate(&query_lower, candidate) else {
            continue;
        };
        let mut score = score + candidate.kind.rank_weight();
        if active_server.is_some() && candidate.server_id == active_server {
            score += 5;
        }
        matches.push(QuickOpenMatch {
            candidate: candidate.clone(),
            score,
        });
    }

    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.candidate.title.cmp(&right.candidate.title))
            .then_with(|| left.candidate.node_id.cmp(&right.candidate.node_id))
    });
    matches.truncate(MAX_RESULTS);
    matches
}

fn score_candidate(query_lower: &str, candidate: &QuickOpenCandidate) -> Option<i32> {
    let mut best = CommandPaletteMatcher::subsequence_score(query_lower, &candidate.title_lower);
    let name_lower = candidate.name.to_lowercase();
    if let Some(score) = CommandPaletteMatcher::subsequence_score(query_lower, &name_lower) {
        best = Some(best.map_or(score, |previous| previous.max(score)));
    }
    if query_lower.contains('.')
        && let Some(schema) = &candidate.schema
    {
        let qualified = format!("{}.{}", schema.to_lowercase(), name_lower);
        if let Some(score) = CommandPaletteMatcher::subsequence_score(query_lower, &qualified) {
            best = Some(best.map_or(score, |previous| previous.max(score)));
        }
    }
    if best.is_some() {
        return best;
    }

    let mut fallback: Option<i32> = None;
    if let Some(database) = &candidate.database
        && let Some(score) =
            CommandPaletteMatcher::subsequence_score(query_lower, &database.to_lowercase())
    {
        fallback = Some(score - 40);
    }
    if let Some(score) =
        CommandPaletteMatcher::subsequence_score(query_lower, &candidate.server_name.to_lowercase())
    {
        let penalized = score - 40;
        fallback = Some(fallback.map_or(penalized, |previous| previous.max(penalized)));
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(
        kind: QuickOpenKind,
        title: &str,
        db: Option<&str>,
        server: &str,
    ) -> QuickOpenCandidate {
        cand_id(kind, title, db, server, &format!("{server}:{title}"))
    }

    fn cand_id(
        kind: QuickOpenKind,
        title: &str,
        db: Option<&str>,
        server: &str,
        node_id: &str,
    ) -> QuickOpenCandidate {
        let name = match kind {
            QuickOpenKind::Table
            | QuickOpenKind::View
            | QuickOpenKind::Procedure
            | QuickOpenKind::Function => title.rsplit('.').next().unwrap_or(title),
            _ => title,
        };
        let schema = match kind {
            QuickOpenKind::Table
            | QuickOpenKind::View
            | QuickOpenKind::Procedure
            | QuickOpenKind::Function => {
                title.split_once('.').map(|(schema, _)| schema.to_string())
            }
            _ => None,
        };
        QuickOpenCandidate {
            node_id: node_id.to_string(),
            kind,
            server_id: Some(uuid_for(server)),
            server_name: server.to_string(),
            database: db.map(str::to_string),
            schema,
            name: name.to_string(),
            title: title.to_string(),
            title_lower: title.to_lowercase(),
        }
    }

    fn uuid_for(server: &str) -> Uuid {
        match server {
            "A" => Uuid::from_u128(1),
            "B" => Uuid::from_u128(2),
            _ => Uuid::from_u128(server.len() as u128 + 10),
        }
    }

    #[test]
    fn empty_query_returns_tree_order_capped() {
        let candidates: Vec<_> = (0..120)
            .map(|i| {
                cand(
                    QuickOpenKind::Table,
                    &format!("dbo.T{i:03}"),
                    Some("db"),
                    "S",
                )
            })
            .collect();
        let matches = rank("", &candidates, None);
        assert_eq!(matches.len(), MAX_RESULTS);
        for (i, m) in matches.iter().enumerate() {
            assert_eq!(m.candidate.title, format!("dbo.T{i:03}"));
            assert_eq!(m.score, 0);
        }
    }

    #[test]
    fn results_are_capped_after_ranking() {
        let mut candidates: Vec<_> = (0..200)
            .map(|i| {
                cand(
                    QuickOpenKind::Table,
                    &format!("dbo.ord{i:03}"),
                    Some("db"),
                    "S",
                )
            })
            .collect();
        candidates.insert(
            50,
            cand_id(
                QuickOpenKind::Table,
                "dbo.ord",
                Some("db"),
                "S",
                "exact-ord",
            ),
        );
        let matches = rank("ord", &candidates, None);
        assert_eq!(matches.len(), MAX_RESULTS);
        assert_eq!(matches[0].candidate.title, "dbo.ord");
        assert_eq!(matches[0].candidate.node_id, "exact-ord");
    }

    #[test]
    fn exact_name_outranks_substring() {
        let candidates = vec![
            cand(QuickOpenKind::Table, "dbo.OrderLines", Some("db"), "S"),
            cand(QuickOpenKind::Table, "dbo.Orders", Some("db"), "S"),
        ];
        let matches = rank("orders", &candidates, None);
        assert_eq!(matches[0].candidate.title, "dbo.Orders");
        assert!(
            matches
                .iter()
                .any(|m| m.candidate.title == "dbo.OrderLines")
        );
        assert!(matches[0].score > matches[1].score);
    }

    #[test]
    fn schema_qualified_query_matches_schema_dot_name() {
        let candidates = vec![
            cand(QuickOpenKind::Table, "dbo.Orders", Some("db"), "S"),
            cand(QuickOpenKind::Table, "sales.Orders", Some("db"), "S"),
        ];
        let matches = rank("sales.ord", &candidates, None);
        assert!(matches.iter().any(|m| m.candidate.title == "sales.Orders"));
        assert!(!matches.iter().any(|m| m.candidate.title == "dbo.Orders"));
    }

    #[test]
    fn database_and_server_match_with_penalty() {
        let products = cand(QuickOpenKind::Table, "dbo.Products", Some("sales"), "Other");
        let via_server = cand(QuickOpenKind::Table, "dbo.Customers", Some("sales"), "Prod");
        let candidates = vec![products, via_server];
        let matches = rank("prod", &candidates, None);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].candidate.title, "dbo.Products");
        assert_eq!(matches[1].candidate.title, "dbo.Customers");
        assert!(matches[0].score > matches[1].score);
    }

    #[test]
    fn active_server_wins_ties() {
        let on_a = cand_id(
            QuickOpenKind::Table,
            "dbo.Orders",
            Some("db"),
            "A",
            "a:orders",
        );
        let on_b = cand_id(
            QuickOpenKind::Table,
            "dbo.Orders",
            Some("db"),
            "B",
            "b:orders",
        );
        let candidates = vec![on_a, on_b];
        let with_b = rank("orders", &candidates, Some(Uuid::from_u128(2)));
        assert_eq!(with_b[0].candidate.server_name, "B");
        let unscoped = rank("orders", &candidates, None);
        assert_eq!(unscoped[0].candidate.node_id, "a:orders");
        assert_eq!(unscoped[1].candidate.node_id, "b:orders");
    }

    #[test]
    fn tables_outrank_databases_on_equal_text() {
        let candidates = vec![
            cand(QuickOpenKind::Database, "orders", None, "S"),
            cand(QuickOpenKind::Table, "dbo.orders", Some("sales"), "S"),
        ];
        let matches = rank("orders", &candidates, None);
        assert_eq!(matches[0].candidate.kind, QuickOpenKind::Table);
        assert_eq!(matches[1].candidate.kind, QuickOpenKind::Database);
    }

    #[test]
    fn no_match_returns_empty() {
        let candidates = vec![cand(QuickOpenKind::Table, "dbo.Orders", Some("db"), "S")];
        assert!(rank("zzz", &candidates, None).is_empty());
    }
}
