//! Deterministic schema-graph expansion and join-path discovery.
//!
//! Operates over the FK relationships already stored in `catalog_relationship`.
//! No vector search, no LLM calls, no new dependencies.

use std::collections::{HashMap, HashSet};

use crate::catalog::store::{CatalogError, CatalogStore, NeighborDirection, NeighborEdge};

#[derive(Debug, Clone, PartialEq)]
pub struct GraphExpansionOptions {
    pub max_depth: usize,
    pub max_neighbors_per_hop: usize,
    pub max_total: usize,
    /// Whether name-inferred edges participate in expansion at all.
    pub include_inferred: bool,
    /// Inferred edges below this confidence are dropped. Declared FKs
    /// (confidence `1.0`) always pass.
    pub min_confidence: f64,
}

impl Default for GraphExpansionOptions {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_neighbors_per_hop: 12,
            max_total: 20,
            include_inferred: true,
            min_confidence: 0.40,
        }
    }
}

impl GraphExpansionOptions {
    pub fn for_purpose(purpose: crate::catalog::card::SchemaPurpose) -> Self {
        match purpose {
            crate::catalog::card::SchemaPurpose::SqlGeneration
            | crate::catalog::card::SchemaPurpose::SqlReview => Self {
                max_depth: 2,
                max_total: 20,
                include_inferred: true,
                min_confidence: 0.55,
                ..Self::default()
            },
            crate::catalog::card::SchemaPurpose::Explanation => Self {
                max_depth: 2,
                max_total: 20,
                include_inferred: true,
                min_confidence: 0.40,
                ..Self::default()
            },
            crate::catalog::card::SchemaPurpose::SafetyCheck => Self {
                max_depth: 1,
                max_total: 10,
                include_inferred: false,
                min_confidence: 1.0,
                ..Self::default()
            },
            crate::catalog::card::SchemaPurpose::General => Self::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpandedNode {
    pub object_key: String,
    pub depth: usize,
    pub via: Option<NeighborEdge>,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpansionResult {
    pub direct: Vec<String>,
    pub expanded: Vec<ExpandedNode>,
    pub all_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JoinPath {
    pub steps: Vec<JoinPathStep>,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JoinPathStep {
    pub from_object_key: String,
    pub from_columns: Vec<String>,
    pub to_object_key: String,
    pub to_columns: Vec<String>,
    pub direction: JoinDirection,
    /// True when this step traverses a name-inferred edge rather than a
    /// declared foreign key.
    #[serde(default)]
    pub is_inferred: bool,
    /// Confidence of the weakest edge in this step. Declared foreign keys are
    /// `1.0`; name-inferred steps carry their inference confidence.
    pub confidence: f64,
}

/// Controls how [`find_join_paths_with_options`] treats name-inferred edges.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinPathOptions {
    pub max_depth: usize,
    /// When `false`, name-inferred edges are skipped entirely and only declared
    /// foreign keys are traversed (strict mode for safety-critical callers).
    pub include_inferred: bool,
    /// Inferred edges below this confidence are skipped. Declared FKs always
    /// pass. The default `0.0` traverses every persisted inferred edge,
    /// preserving the historical behavior of [`find_join_paths`].
    pub min_confidence: f64,
}

impl Default for JoinPathOptions {
    fn default() -> Self {
        Self {
            max_depth: 4,
            include_inferred: true,
            min_confidence: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinDirection {
    Forward,
    Reverse,
}

fn resolve_target_key(edge: &NeighborEdge) -> String {
    edge.to_object_key
        .clone()
        .unwrap_or_else(|| derive_key_from_namespace_name(&edge.to_namespace, &edge.to_name))
}

fn neighbor_key(edge: &NeighborEdge) -> String {
    match edge.direction {
        NeighborDirection::Outbound => resolve_target_key(edge),
        NeighborDirection::Inbound => edge.from_object_key.clone(),
    }
}

fn derive_key_from_namespace_name(namespace: &Option<String>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{}.{}", ns.to_ascii_lowercase(), name.to_ascii_lowercase()),
        None => name.to_ascii_lowercase(),
    }
}

pub fn expand_schema_graph(
    store: &CatalogStore,
    seeds: &[String],
    opts: &GraphExpansionOptions,
) -> Result<ExpansionResult, CatalogError> {
    let live_keys = store.live_object_key_set()?;
    let mut visited: HashSet<String> = HashSet::new();
    let mut all_keys: Vec<String> = Vec::new();
    let mut expanded: Vec<ExpandedNode> = Vec::new();

    for seed in seeds.iter().take(opts.max_total) {
        if visited.insert(seed.clone()) {
            all_keys.push(seed.clone());
        }
    }

    let mut frontier: Vec<String> = all_keys.clone();

    for depth in 1..=opts.max_depth {
        if frontier.is_empty() || all_keys.len() >= opts.max_total {
            break;
        }

        let edges = store.neighbor_edges(&frontier)?;

        let mut candidates: Vec<(String, NeighborEdge, f64)> = edges
            .into_iter()
            .filter_map(|edge| {
                if !edge_passes_filter(&edge, opts) {
                    return None;
                }
                let neighbor = neighbor_key(&edge);
                if visited.contains(&neighbor) {
                    return None;
                }
                if !live_keys.contains(&neighbor) {
                    return None;
                }
                let score = score_edge_for_expansion(&edge, depth);
                Some((neighbor, edge, score))
            })
            .collect();

        candidates.sort_by(|a, b| compare_candidate(b, a));

        let remaining = opts.max_total.saturating_sub(all_keys.len());
        let take = opts.max_neighbors_per_hop.min(remaining);

        let mut next_frontier: Vec<String> = Vec::new();

        for (object_key, edge, score) in candidates.into_iter().take(take) {
            if visited.insert(object_key.clone()) {
                all_keys.push(object_key.clone());
                next_frontier.push(object_key.clone());
                expanded.push(ExpandedNode {
                    object_key,
                    depth,
                    via: Some(edge),
                    score,
                });
            }
        }

        frontier = next_frontier;
    }

    Ok(ExpansionResult {
        direct: seeds.to_vec(),
        expanded,
        all_keys,
    })
}

pub fn find_join_paths(
    store: &CatalogStore,
    source: &str,
    target: &str,
    max_depth: usize,
) -> Result<Vec<JoinPath>, CatalogError> {
    find_join_paths_with_options(
        store,
        source,
        target,
        &JoinPathOptions {
            max_depth,
            ..JoinPathOptions::default()
        },
    )
}

/// Join-path discovery with explicit control over name-inferred edges.
///
/// Declared foreign keys are always traversed. Inferred edges participate only
/// when `opts.include_inferred` is set and their confidence is at or above
/// `opts.min_confidence`. Declared-only paths always rank above paths that
/// contain any inferred step.
pub fn find_join_paths_with_options(
    store: &CatalogStore,
    source: &str,
    target: &str,
    opts: &JoinPathOptions,
) -> Result<Vec<JoinPath>, CatalogError> {
    if source == target {
        return Ok(vec![JoinPath {
            steps: vec![],
            score: 1.0,
        }]);
    }

    let max_depth = opts.max_depth.clamp(1, 6);
    let max_paths = 3;
    let mut paths: Vec<JoinPath> = Vec::new();
    let live_keys = store.live_object_key_set()?;

    let mut current_level: Vec<PartialPath> = vec![PartialPath::at(source)];

    let mut best_depth_by_key: HashMap<String, usize> = HashMap::new();
    best_depth_by_key.insert(source.to_string(), 0);

    while paths.len() < max_paths {
        if current_level.is_empty() {
            break;
        }
        let depth = current_level[0].depth;
        if depth >= max_depth {
            break;
        }

        let frontier: Vec<String> = current_level
            .iter()
            .map(|path| path.last_key().to_string())
            .collect();
        let all_edges: Vec<NeighborEdge> = store
            .neighbor_edges(&frontier)?
            .into_iter()
            .filter(|edge| {
                inferred_edge_admissible(edge, opts.include_inferred, opts.min_confidence)
            })
            .collect();

        let mut next_level: Vec<PartialPath> = Vec::new();

        for path in current_level {
            let path_key = path.last_key();
            let path_edges: Vec<NeighborEdge> = all_edges
                .iter()
                .filter(|edge| edge_incident_to(edge, path_key))
                .cloned()
                .collect();
            let grouped = group_edges_by_fk_target(&path_edges);

            for (_group_key, group_edges) in grouped {
                let representative = &group_edges[0];
                let next = neighbor_key(representative);

                if !live_keys.contains(&next) {
                    continue;
                }

                if path.contains(&next) {
                    continue;
                }

                let next_path = path.clone().extend_group(&group_edges, next.clone());

                if next == target {
                    paths.push(next_path.into_join_path());
                    continue;
                }

                let next_depth = next_path.depth;
                let dominated = best_depth_by_key
                    .get(&next)
                    .is_some_and(|seen| next_depth > *seen);
                if dominated {
                    continue;
                }

                best_depth_by_key.insert(next.clone(), next_depth);
                next_level.push(next_path);
            }
        }

        current_level = next_level;
    }

    paths.sort_by(compare_join_paths);
    paths.truncate(max_paths);
    Ok(paths)
}

fn edge_incident_to(edge: &NeighborEdge, key: &str) -> bool {
    match edge.direction {
        NeighborDirection::Outbound => edge.from_object_key == key,
        NeighborDirection::Inbound => {
            edge.to_object_key.as_deref().is_some_and(|to| to == key)
                || derive_key_from_namespace_name(&edge.to_namespace, &edge.to_name) == key
        }
    }
}

fn group_edges_by_fk_target(edges: &[NeighborEdge]) -> Vec<(String, Vec<NeighborEdge>)> {
    let mut groups: HashMap<String, Vec<NeighborEdge>> = HashMap::new();
    for edge in edges {
        let key = format!(
            "{}:{}:{}",
            edge.from_object_key,
            neighbor_key(edge),
            match edge.direction {
                NeighborDirection::Outbound => "fwd",
                NeighborDirection::Inbound => "rev",
            }
        );
        groups.entry(key).or_default().push(edge.clone());
    }
    let mut result: Vec<(String, Vec<NeighborEdge>)> = groups.into_iter().collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn score_edge_for_expansion(edge: &NeighborEdge, depth: usize) -> f64 {
    let mut score = 1.0 / (depth as f64);

    match edge.direction {
        NeighborDirection::Outbound => score += 0.1,
        NeighborDirection::Inbound => score += 0.05,
    }

    if edge.to_object_key.is_some() {
        score += 0.2;
    }

    let lower = edge.to_name.to_ascii_lowercase();
    if lower.contains("audit")
        || lower.contains("log")
        || lower.contains("history")
        || lower.contains("archive")
    {
        score *= 0.5;
    }

    // Demote name-inferred edges: declared FKs are authoritative and should
    // always outrank a same-shape inferred guess.
    if edge.is_inferred {
        score *= 0.60 * edge.confidence;
    }

    score
}

/// Core admissibility rule for name-inferred edges, shared by graph expansion
/// and join-path discovery. Declared edges always pass; inferred edges pass
/// only when inference is enabled and confidence clears the threshold.
fn inferred_edge_admissible(
    edge: &NeighborEdge,
    include_inferred: bool,
    min_confidence: f64,
) -> bool {
    if !edge.is_inferred {
        return true;
    }
    include_inferred && edge.confidence >= min_confidence
}

/// An edge is admissible for expansion when it is either declared, or inferred
/// with inference enabled and a confidence at or above the option threshold.
fn edge_passes_filter(edge: &NeighborEdge, opts: &GraphExpansionOptions) -> bool {
    inferred_edge_admissible(edge, opts.include_inferred, opts.min_confidence)
}

fn compare_candidate(
    a: &(String, NeighborEdge, f64),
    b: &(String, NeighborEdge, f64),
) -> std::cmp::Ordering {
    a.2.partial_cmp(&b.2)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.0.cmp(&b.0))
}

fn compare_join_paths(a: &JoinPath, b: &JoinPath) -> std::cmp::Ordering {
    // Declared-only paths rank above any path containing inferred edges.
    let a_inferred = a.steps.iter().filter(|s| s.is_inferred).count();
    let b_inferred = b.steps.iter().filter(|s| s.is_inferred).count();
    a_inferred
        .cmp(&b_inferred)
        .then_with(|| a.steps.len().cmp(&b.steps.len()))
        // Among ties, prefer the path whose weakest step is most confident.
        .then_with(|| {
            path_min_confidence(b)
                .partial_cmp(&path_min_confidence(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// The confidence of a path is the confidence of its weakest step. An
/// all-declared path is `1.0`.
fn path_min_confidence(path: &JoinPath) -> f64 {
    path.steps
        .iter()
        .map(|s| s.confidence)
        .fold(1.0_f64, f64::min)
}

#[derive(Clone)]
struct PartialPath {
    keys: Vec<String>,
    steps: Vec<JoinPathStep>,
    depth: usize,
}

impl PartialPath {
    fn at(key: &str) -> Self {
        Self {
            keys: vec![key.to_string()],
            steps: vec![],
            depth: 0,
        }
    }

    fn last_key(&self) -> &str {
        self.keys.last().map(|s| s.as_str()).unwrap_or("")
    }

    fn contains(&self, key: &str) -> bool {
        self.keys.contains(&key.to_string())
    }

    fn extend_group(mut self, edges: &[NeighborEdge], next: String) -> Self {
        let representative = &edges[0];
        let to_key = resolve_target_key(representative);
        let direction = match representative.direction {
            NeighborDirection::Outbound => JoinDirection::Forward,
            NeighborDirection::Inbound => JoinDirection::Reverse,
        };

        let from_columns: Vec<String> = edges.iter().map(|e| e.from_column.clone()).collect();
        let to_columns: Vec<String> = edges.iter().filter_map(|e| e.to_column.clone()).collect();
        let is_inferred = edges.iter().any(|e| e.is_inferred);
        // Weakest edge in the group; declared FKs are 1.0.
        let confidence = edges.iter().map(|e| e.confidence).fold(1.0_f64, f64::min);

        self.steps.push(JoinPathStep {
            from_object_key: representative.from_object_key.clone(),
            from_columns,
            to_object_key: to_key,
            to_columns,
            direction,
            is_inferred,
            confidence,
        });

        if !self.keys.contains(&next) {
            self.keys.push(next);
        }
        self.depth += 1;
        self
    }

    fn into_join_path(self) -> JoinPath {
        let step_count = self.steps.len();
        let base = 1.0 / (step_count as f64).max(1.0);
        // Fold the weakest step's confidence into the score so an inferred path
        // never scores as high as a same-length declared path.
        let min_conf = self
            .steps
            .iter()
            .map(|s| s.confidence)
            .fold(1.0_f64, f64::min);
        JoinPath {
            steps: self.steps,
            score: base * min_conf,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::CatalogStore;
    use crate::catalog::wire::{
        CatalogColumn, CatalogForeignKey, CatalogObject, CatalogObjectKind, CatalogSource,
        SourceKind,
    };

    fn src() -> CatalogSource {
        CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: None,
            modify_date: None,
        }
    }

    fn table(
        name: &str,
        schema: Option<&str>,
        cols: &[&str],
        fks: &[CatalogForeignKey],
    ) -> CatalogObject {
        CatalogObject {
            schema: schema.map(|s| s.to_string()),
            name: name.to_string(),
            kind: CatalogObjectKind::Table,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| CatalogColumn {
                    name: (*c).into(),
                    type_name: Some("INT".into()),
                    ordinal: i as i64 + 1,
                    nullable: false,
                })
                .collect(),
            primary_key: vec![],
            foreign_keys: fks.to_vec(),
            parameters: vec![],
            definition: None,
            source: src(),
        }
    }

    fn fk(
        columns: &[&str],
        target_schema: Option<&str>,
        target_table: &str,
        target_columns: &[&str],
    ) -> CatalogForeignKey {
        CatalogForeignKey {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            target_schema: target_schema.map(|s| s.to_string()),
            target_table: target_table.to_string(),
            target_columns: target_columns.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sync_all(store: &CatalogStore, objects: &[CatalogObject]) {
        store.sync(objects).unwrap();
    }

    #[test]
    fn expansion_includes_fk_target() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.orders".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        assert!(
            result.all_keys.contains(&"dbo.customers".to_string()),
            "expanded keys should include FK target customers, got {:?}",
            result.all_keys
        );
    }

    #[test]
    fn expansion_includes_inbound_references() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("customers", Some("dbo"), &["id", "name"], &[]),
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
            ],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.customers".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        assert!(
            result.all_keys.contains(&"dbo.orders".to_string()),
            "expanded keys should include inbound referrer orders, got {:?}",
            result.all_keys
        );
    }

    #[test]
    fn depth_2_includes_transitive_neighbor() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "order_lines",
                    Some("dbo"),
                    &["id", "order_id"],
                    &[fk(&["order_id"], Some("dbo"), "orders", &["id"])],
                ),
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let opts = GraphExpansionOptions {
            max_depth: 2,
            ..GraphExpansionOptions::default()
        };
        let result = expand_schema_graph(&store, &["dbo.order_lines".to_string()], &opts).unwrap();
        assert!(
            result.all_keys.contains(&"dbo.customers".to_string()),
            "depth-2 should reach customers via orders, got {:?}",
            result.all_keys
        );
    }

    #[test]
    fn cycles_do_not_duplicate_nodes() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "a",
                    Some("dbo"),
                    &["id", "b_id"],
                    &[fk(&["b_id"], Some("dbo"), "b", &["id"])],
                ),
                table(
                    "b",
                    Some("dbo"),
                    &["id", "a_id"],
                    &[fk(&["a_id"], Some("dbo"), "a", &["id"])],
                ),
            ],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.a".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        let a_count = result.all_keys.iter().filter(|k| *k == "dbo.a").count();
        assert_eq!(
            a_count, 1,
            "should not revisit a, got {:?}",
            result.all_keys
        );
    }

    #[test]
    fn max_total_is_enforced() {
        let store = CatalogStore::open_in_memory().unwrap();
        let objects: Vec<CatalogObject> = (0..10)
            .map(|i| {
                let parent = format!("parent_{i}");
                table(
                    &format!("child_{i}"),
                    Some("dbo"),
                    &["id", "parent_id"],
                    &[fk(&["parent_id"], Some("dbo"), &parent, &["id"])],
                )
            })
            .chain((0..10).map(|i| table(&format!("parent_{i}"), Some("dbo"), &["id"], &[])))
            .collect();
        sync_all(&store, &objects);

        let opts = GraphExpansionOptions {
            max_total: 5,
            ..GraphExpansionOptions::default()
        };
        let result = expand_schema_graph(&store, &["dbo.child_0".to_string()], &opts).unwrap();
        assert!(
            result.all_keys.len() <= 5,
            "all_keys should be capped at max_total=5, got {}",
            result.all_keys.len()
        );
    }

    #[test]
    fn expansion_skips_orphaned_fk_targets() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[table(
                "child",
                Some("dbo"),
                &["id", "orphan_ref"],
                &[fk(&["orphan_ref"], Some("dbo"), "orphan_table", &["id"])],
            )],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.child".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        assert!(
            !result.all_keys.contains(&"dbo.orphan_table".to_string()),
            "orphaned FK target (not in catalog) should not appear in expansion, got {:?}",
            result.all_keys
        );
        assert_eq!(
            result.expanded.len(),
            0,
            "no expanded nodes when target is orphaned"
        );
    }

    #[test]
    fn join_path_direct_fk() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.orders", "dbo.customers", 4).unwrap();
        assert!(!paths.is_empty(), "should find at least one path");
        assert_eq!(
            paths[0].steps.len(),
            1,
            "direct FK should be one step, got {:?}",
            paths[0].steps
        );
    }

    #[test]
    fn join_path_through_bridge() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "order_lines",
                    Some("dbo"),
                    &["id", "order_id"],
                    &[fk(&["order_id"], Some("dbo"), "orders", &["id"])],
                ),
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.order_lines", "dbo.customers", 4).unwrap();
        assert!(!paths.is_empty(), "should find at least one path");
        assert_eq!(
            paths[0].steps.len(),
            2,
            "bridge path should be two steps, got {:?}",
            paths[0].steps
        );
    }

    #[test]
    fn join_path_no_path_returns_empty() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("a", Some("dbo"), &["id"], &[]),
                table("b", Some("dbo"), &["id"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.a", "dbo.b", 4).unwrap();
        assert!(
            paths.is_empty(),
            "disconnected tables should yield no paths"
        );
    }

    #[test]
    fn join_path_same_object() {
        let store = CatalogStore::open_in_memory().unwrap();
        let paths = find_join_paths(&store, "dbo.a", "dbo.a", 4).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].steps.is_empty());
    }

    #[test]
    fn join_path_orphaned_target_returns_empty() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[table(
                "orders",
                Some("dbo"),
                &["id", "customer_id"],
                &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
            )],
        );

        let paths = find_join_paths(&store, "dbo.orders", "dbo.customers", 4).unwrap();
        assert!(
            paths.is_empty(),
            "orphaned FK target (not in catalog) should yield no join path"
        );
    }

    #[test]
    fn join_path_composite_fk_preserves_all_columns() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "child",
                    Some("dbo"),
                    &["id", "col_a", "col_b"],
                    &[fk(
                        &["col_a", "col_b"],
                        Some("dbo"),
                        "parent",
                        &["parent_a", "parent_b"],
                    )],
                ),
                table("parent", Some("dbo"), &["id", "parent_a", "parent_b"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.child", "dbo.parent", 4).unwrap();
        assert!(!paths.is_empty(), "should find composite FK path");
        let step = &paths[0].steps[0];
        assert_eq!(
            step.from_columns,
            vec!["col_a", "col_b"],
            "composite FK from_columns should be preserved"
        );
        assert_eq!(
            step.to_columns,
            vec!["parent_a", "parent_b"],
            "composite FK to_columns should be preserved"
        );
    }

    #[test]
    fn join_path_reverse_direction() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.customers", "dbo.orders", 4).unwrap();
        assert!(!paths.is_empty(), "reverse path should exist");
        assert_eq!(paths[0].steps.len(), 1, "reverse path should be one step");
        assert_eq!(paths[0].steps[0].direction, JoinDirection::Reverse);
    }

    #[test]
    fn expansion_prefers_resolved_targets() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "child",
                    Some("dbo"),
                    &["id", "resolved_ref", "unresolved_ref"],
                    &[
                        fk(&["resolved_ref"], Some("dbo"), "resolved_target", &["id"]),
                        fk(
                            &["unresolved_ref"],
                            Some("other"),
                            "unresolved_target",
                            &["id"],
                        ),
                    ],
                ),
                table("resolved_target", Some("dbo"), &["id"], &[]),
            ],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.child".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        let resolved_score = result
            .expanded
            .iter()
            .find(|n| n.object_key == "dbo.resolved_target")
            .map(|n| n.score);
        assert!(
            resolved_score.is_some(),
            "resolved target should appear in expansion"
        );
        let orphaned = result
            .expanded
            .iter()
            .find(|n| n.object_key == "other.unresolved_target");
        assert!(
            orphaned.is_none(),
            "orphaned target should not appear in expansion"
        );
    }

    #[test]
    fn audit_tables_are_penalized() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id", "audit_id"],
                    &[
                        fk(&["customer_id"], Some("dbo"), "customers", &["id"]),
                        fk(&["audit_id"], Some("dbo"), "audit_log", &["id"]),
                    ],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
                table("audit_log", Some("dbo"), &["id", "message"], &[]),
            ],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.orders".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        let customer_score = result
            .expanded
            .iter()
            .find(|n| n.object_key == "dbo.customers")
            .map(|n| n.score)
            .unwrap_or(0.0);
        let audit_score = result
            .expanded
            .iter()
            .find(|n| n.object_key == "dbo.audit_log")
            .map(|n| n.score)
            .unwrap_or(0.0);
        assert!(
            customer_score > audit_score,
            "customers ({customer_score}) should outrank audit_log ({audit_score})"
        );
    }

    #[test]
    fn expansion_includes_inferred_edge_by_default() {
        let store = CatalogStore::open_in_memory().unwrap();
        // No declared FK; `customer_id` infers a reference to `customers`.
        sync_all(
            &store,
            &[
                table("orders", Some("dbo"), &["id", "customer_id"], &[]),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let result = expand_schema_graph(
            &store,
            &["dbo.orders".to_string()],
            &GraphExpansionOptions::default(),
        )
        .unwrap();
        assert!(
            result.all_keys.contains(&"dbo.customers".to_string()),
            "inferred target should be reached by default, got {:?}",
            result.all_keys
        );
        let node = result
            .expanded
            .iter()
            .find(|n| n.object_key == "dbo.customers")
            .expect("customers node present");
        assert!(node.via.as_ref().is_some_and(|e| e.is_inferred));
    }

    #[test]
    fn expansion_excludes_inferred_edge_when_disabled() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("orders", Some("dbo"), &["id", "customer_id"], &[]),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let opts = GraphExpansionOptions {
            include_inferred: false,
            min_confidence: 1.0,
            ..GraphExpansionOptions::default()
        };
        let result = expand_schema_graph(&store, &["dbo.orders".to_string()], &opts).unwrap();
        assert!(
            !result.all_keys.contains(&"dbo.customers".to_string()),
            "inferred target should be excluded, got {:?}",
            result.all_keys
        );
    }

    #[test]
    fn expansion_excludes_inferred_edge_below_min_confidence() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("orders", Some("dbo"), &["id", "customer_id"], &[]),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        // Confidence here is ~0.70; demand more than that.
        let opts = GraphExpansionOptions {
            include_inferred: true,
            min_confidence: 0.95,
            ..GraphExpansionOptions::default()
        };
        let result = expand_schema_graph(&store, &["dbo.orders".to_string()], &opts).unwrap();
        assert!(
            !result.all_keys.contains(&"dbo.customers".to_string()),
            "below-threshold inferred target should be excluded, got {:?}",
            result.all_keys
        );
    }

    #[test]
    fn join_path_via_inferred_edge() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("orders", Some("dbo"), &["id", "customer_id"], &[]),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.orders", "dbo.customers", 4).unwrap();
        assert!(!paths.is_empty(), "inferred join path should be found");
        assert_eq!(paths[0].steps.len(), 1);
        assert!(
            paths[0].steps[0].is_inferred,
            "the single step should be inferred, got {:?}",
            paths[0].steps
        );
    }

    #[test]
    fn declared_join_path_ranks_above_inferred() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                // order_lines->orders is declared; order_lines.customer_id is an
                // *inferred* shortcut straight to customers.
                table(
                    "order_lines",
                    Some("dbo"),
                    &["id", "order_id", "customer_id"],
                    &[fk(&["order_id"], Some("dbo"), "orders", &["id"])],
                ),
                table(
                    "orders",
                    Some("dbo"),
                    &["id", "customer_id"],
                    &[fk(&["customer_id"], Some("dbo"), "customers", &["id"])],
                ),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        let paths = find_join_paths(&store, "dbo.order_lines", "dbo.customers", 4).unwrap();
        assert!(paths.len() >= 2, "expected both paths, got {:?}", paths);
        // The declared 2-hop path must rank ahead of the 1-hop inferred shortcut.
        assert!(
            paths[0].steps.iter().all(|s| !s.is_inferred),
            "top path should be declared-only, got {:?}",
            paths[0].steps
        );
        assert!(
            paths.iter().any(|p| p.steps.iter().any(|s| s.is_inferred)),
            "inferred shortcut should still be present as a lower-ranked option"
        );
    }

    #[test]
    fn join_path_strict_mode_excludes_inferred_edge() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("orders", Some("dbo"), &["id", "customer_id"], &[]),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        // The only route is the inferred one; strict mode must find nothing.
        let opts = JoinPathOptions {
            max_depth: 4,
            include_inferred: false,
            min_confidence: 1.0,
        };
        let paths =
            find_join_paths_with_options(&store, "dbo.orders", "dbo.customers", &opts).unwrap();
        assert!(
            paths.is_empty(),
            "strict (declared-only) mode should find no path, got {paths:?}"
        );
    }

    #[test]
    fn join_path_min_confidence_filters_inferred_edge() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("orders", Some("dbo"), &["id", "customer_id"], &[]),
                table("customers", Some("dbo"), &["id", "name"], &[]),
            ],
        );

        // Inferred confidence is ~0.70; demanding 0.95 drops the only edge.
        let strict = JoinPathOptions {
            max_depth: 4,
            include_inferred: true,
            min_confidence: 0.95,
        };
        let none =
            find_join_paths_with_options(&store, "dbo.orders", "dbo.customers", &strict).unwrap();
        assert!(none.is_empty(), "above-threshold demand drops the edge");

        // A permissive threshold keeps it, and the step carries its confidence.
        let lenient = JoinPathOptions {
            max_depth: 4,
            include_inferred: true,
            min_confidence: 0.5,
        };
        let found =
            find_join_paths_with_options(&store, "dbo.orders", "dbo.customers", &lenient).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].steps[0].is_inferred);
        assert!(
            found[0].steps[0].confidence > 0.5 && found[0].steps[0].confidence < 1.0,
            "inferred step should carry sub-1.0 confidence, got {}",
            found[0].steps[0].confidence
        );
        // Score is demoted below a declared single hop (1.0).
        assert!(found[0].score < 1.0);
    }
}
