//! Lightweight relevance ranking for picking which objects to send to the LLM.
//!
//! FTS5 provides bounded candidates; this module applies the deterministic
//! term-overlap rerank used by existing tests/callers.

use std::collections::HashSet;

use crate::catalog::embed::DynEmbeddingProvider;
use crate::catalog::hybrid::{HybridSearchOptions, merge_hybrid};
use crate::catalog::store::{CatalogError, CatalogStore, SearchFilter, StoredObject};
use crate::catalog::tokens::tokenize_terms;
use crate::catalog::vector::{VectorSearchResult, vector_search};
use crate::catalog::vector_store::{DynVectorStore, VectorFilter};
use crate::catalog::wire::{CatalogObject, CatalogObjectKind};

/// Rank objects by relevance to `question`, most relevant first.
pub fn rank_for_question(objects: &[StoredObject], question: &str) -> Vec<RankedObject> {
    let qterms = tokenize(question);
    if qterms.is_empty() {
        return objects
            .iter()
            .map(|o| RankedObject {
                object: o.clone(),
                score: 0.0,
            })
            .collect();
    }

    let mut ranked: Vec<RankedObject> = objects
        .iter()
        .map(|o| RankedObject {
            object: o.clone(),
            score: score_object(o, &qterms),
        })
        .collect();

    // Tie-break deterministically by name so ordering is stable across runs.
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.object.name.cmp(&b.object.name))
    });

    ranked
}

/// Search the persisted catalog with FTS5 candidate retrieval, then rerank with
/// the deterministic scorer used by existing tests/callers.
pub fn search_store_objects(
    store: &CatalogStore,
    question: &str,
    limit: usize,
    filter: SearchFilter,
) -> Result<Vec<RankedObject>, CatalogError> {
    let limit = limit.clamp(1, 100);
    let candidate_limit = limit.saturating_mul(3).clamp(20, 100);
    let has_query_terms = !tokenize(question).is_empty();
    let hits = store.search_schema(question, candidate_limit, filter)?;
    let keys: Vec<String> = hits.iter().map(|hit| hit.object_key.clone()).collect();
    let mut objects = store.objects_by_keys(&keys)?;

    if objects.is_empty() {
        if has_query_terms && store.search_index_is_healthy()? {
            return Ok(Vec::new());
        }
        objects = store.live_objects_limited(candidate_limit)?;
    }

    let fts_scores = hits
        .iter()
        .map(|hit| (hit.object_key.as_str(), hit.score))
        .collect::<std::collections::HashMap<_, _>>();
    let qterms = tokenize(question);
    let mut ranked = objects
        .into_iter()
        .map(|object| {
            let fts_score = fts_scores
                .get(object.object_key.as_str())
                .copied()
                .unwrap_or(0.0);
            let term_score = if qterms.is_empty() {
                0.0
            } else {
                score_object(&object, &qterms)
            };
            RankedObject {
                object,
                score: term_score + fts_score,
            }
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.object.name.cmp(&b.object.name))
            .then_with(|| a.object.object_key.cmp(&b.object.object_key))
    });
    ranked.truncate(limit);
    Ok(ranked)
}

/// An object with its relevance score.
#[derive(Debug, Clone)]
pub struct RankedObject {
    pub object: StoredObject,
    pub score: f64,
}

/// Search the catalog using hybrid FTS + vector search when embeddings exist,
/// falling back to FTS-only when no embeddings or provider are available.
///
/// Preserves the existing `search_store_objects` behavior when no embeddings
/// have been built. When embeddings exist and a provider is given, embeds the
/// query, runs vector search, and merges results using normalized scoring.
pub async fn search_store_objects_hybrid(
    store: &CatalogStore,
    question: &str,
    limit: usize,
    filter: SearchFilter,
    provider: Option<&DynEmbeddingProvider>,
    vector_store: Option<&DynVectorStore>,
) -> Result<Vec<RankedObject>, CatalogError> {
    let fts_results = search_store_objects(store, question, limit, filter)?;

    let provider = match provider {
        Some(p) => p,
        None => return Ok(fts_results),
    };

    let active_provider_id = provider.provider_id();
    let has_vectors = match vector_store {
        Some(vs) => vs.count(Some(VectorFilter::default())).await.unwrap_or(0) > 0,
        None => store.live_embedding_count(active_provider_id)? > 0,
    };
    if !has_vectors {
        return Ok(fts_results);
    }

    let query_embedding = match provider.embed(question).await {
        Ok(v) => v,
        Err(_) => return Ok(fts_results),
    };

    let vector_limit = limit.clamp(1, 100).saturating_mul(3).clamp(20, 100);
    let vector_hits = match vector_store {
        Some(vs) => {
            let filter = VectorFilter::default();
            match vs.search(query_embedding, vector_limit, Some(filter)).await {
                Ok(hits) => hits
                    .into_iter()
                    .map(|h| VectorSearchResult {
                        object_key: h.object_key,
                        score: h.score,
                    })
                    .collect(),
                Err(_) => return Ok(fts_results),
            }
        }
        None => match vector_search(store, &query_embedding, vector_limit, active_provider_id) {
            Ok(hits) => hits,
            Err(_) => return Ok(fts_results),
        },
    };

    if vector_hits.is_empty() {
        return Ok(fts_results);
    }

    let hybrid = merge_hybrid(&fts_results, &vector_hits, &HybridSearchOptions::default());

    let vector_only_keys: Vec<String> = hybrid
        .iter()
        .filter(|c| c.fts_score == 0.0 && c.vector_score > 0.0)
        .filter(|c| {
            !fts_results
                .iter()
                .any(|f| f.object.object_key == c.object_key)
        })
        .map(|c| c.object_key.clone())
        .collect();

    let mut enriched = hybrid;
    if !vector_only_keys.is_empty() {
        let extra_objects = store.objects_by_keys(&vector_only_keys)?;
        let extra_by_key: std::collections::HashMap<String, StoredObject> = extra_objects
            .into_iter()
            .map(|o| (o.object_key.clone(), o))
            .collect();
        for candidate in &mut enriched {
            if candidate.object.object_hash.is_empty()
                && let Some(obj) = extra_by_key.get(&candidate.object_key)
            {
                candidate.object = obj.clone();
            }
        }
    }

    let ranked: Vec<RankedObject> = enriched
        .into_iter()
        .take(limit)
        .map(|c| RankedObject {
            object: c.object,
            score: c.score,
        })
        .collect();

    Ok(ranked)
}

/// Return only objects whose score is above zero (i.e. they share at least one
/// term with the question), preserving rank order.
pub fn relevant_only(ranked: &[RankedObject]) -> Vec<&StoredObject> {
    ranked
        .iter()
        .filter(|r| r.score > 0.0)
        .map(|r| &r.object)
        .collect()
}

fn score_object(obj: &StoredObject, qterms: &HashSet<String>) -> f64 {
    let parsed: Option<CatalogObject> = serde_json::from_str(&obj.body_json).ok();
    let mut score = 0.0f64;
    let mut matched_name = false;

    // Object name (heavily weighted).
    for tok in tokenize(&obj.name) {
        if qterms.contains(&tok) {
            score += 3.0;
            matched_name = true;
        }
    }

    if let Some(parsed) = &parsed {
        // Columns.
        for col in &parsed.columns {
            for tok in tokenize(&col.name) {
                if qterms.contains(&tok) {
                    score += 1.0;
                }
            }
        }
        // Foreign-key targets — joinable tables get a relevance boost.
        for fk in &parsed.foreign_keys {
            for tok in tokenize(&fk.target_table) {
                if qterms.contains(&tok) {
                    score += 2.0;
                }
            }
        }
        // Definition text is a weaker signal.
        if let Some(def) = &parsed.definition {
            for tok in tokenize(def) {
                if qterms.contains(&tok) {
                    score += 0.25;
                }
            }
        }
    }

    // Schema/namespace match is a mild boost.
    if let Some(ns) = &obj.namespace {
        for tok in tokenize(ns) {
            if qterms.contains(&tok) {
                score += 0.5;
            }
        }
    }

    // Procedures/functions answering by name are strong hits.
    if matches!(
        obj.kind,
        CatalogObjectKind::StoredProcedure | CatalogObjectKind::Function
    ) && matched_name
    {
        score += 1.0;
    }

    score
}

/// Lowercase alphanumeric tokenization. Splits on non-alphanumeric and drops
/// tiny tokens and SQL noise words so they don't dominate scoring.
pub fn tokenize(s: &str) -> HashSet<String> {
    tokenize_terms(s).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::CatalogStore;
    use crate::catalog::wire::{
        CatalogColumn, CatalogForeignKey, CatalogObject as WireObject, CatalogObjectKind,
        CatalogSource, SourceKind,
    };

    fn db_source() -> CatalogSource {
        CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: Some(1),
            modify_date: Some(2),
        }
    }

    fn table(name: &str, cols: &[&str], fks: &[(&str, &str)]) -> WireObject {
        WireObject {
            schema: Some("dbo".into()),
            name: name.into(),
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
            foreign_keys: fks
                .iter()
                .map(|(from, target)| CatalogForeignKey {
                    columns: vec![(*from).into()],
                    target_schema: Some("dbo".into()),
                    target_table: (*target).into(),
                    target_columns: vec!["id".into()],
                })
                .collect(),
            parameters: vec![],
            definition: None,
            source: db_source(),
        }
    }

    fn sync_all(store: &CatalogStore, objs: &[WireObject]) {
        store.sync(objs).unwrap();
    }

    #[test]
    fn ranks_object_named_in_question_first() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("customers", &["id", "name"], &[]),
                table(
                    "orders",
                    &["id", "customer_id"],
                    &[("customer_id", "customers")],
                ),
            ],
        );
        let objs = store.live_objects().unwrap();
        let ranked = rank_for_question(&objs, "tell me about customers");
        let names: Vec<&str> = ranked.iter().map(|r| r.object.name.as_str()).collect();
        assert_eq!(names[0], "customers");
    }

    #[test]
    fn join_target_gets_boosted() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table(
                    "orders",
                    &["id", "customer_id"],
                    &[("customer_id", "customers")],
                ),
                table("products", &["id", "sku"], &[]),
            ],
        );
        let objs = store.live_objects().unwrap();
        // Question mentions customers — orders (which FKs to customers) should
        // outrank products.
        let ranked = rank_for_question(&objs, "how do orders relate to customers");
        let names: Vec<&str> = ranked.iter().map(|r| r.object.name.as_str()).collect();
        assert_eq!(names[0], "orders");
        // customers is an FK target of orders → boosted, should beat products.
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("orders") < pos("products"));
    }

    #[test]
    fn column_match_contributes_to_score() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("users", &["id", "email"], &[]),
                table("logs", &["id", "message"], &[]),
            ],
        );
        let objs = store.live_objects().unwrap();
        let ranked = rank_for_question(&objs, "where is the email stored");
        assert_eq!(ranked[0].object.name, "users");
    }

    #[test]
    fn empty_question_returns_all_unranked() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(&store, &[table("a", &["id"], &[])]);
        let objs = store.live_objects().unwrap();
        let ranked = rank_for_question(&objs, "");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].score, 0.0);
    }

    #[test]
    fn relevant_only_filters_zero_score() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[table("users", &["id"], &[]), table("widgets", &["id"], &[])],
        );
        let objs = store.live_objects().unwrap();
        let ranked = rank_for_question(&objs, "users");
        let rel = relevant_only(&ranked);
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].name, "users");
    }

    #[test]
    fn tokenize_drops_sql_noise() {
        let toks = tokenize("SELECT all the FROM users WHERE id");
        assert!(toks.contains("users"));
        assert!(!toks.contains("select"));
        assert!(!toks.contains("the"));
    }

    #[test]
    fn store_search_finds_column_match() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("users", &["id", "email_address"], &[]),
                table("logs", &["id", "message"], &[]),
            ],
        );
        let ranked =
            search_store_objects(&store, "email address", 10, SearchFilter::default()).unwrap();
        assert_eq!(ranked[0].object.name, "users");
    }

    #[test]
    fn store_search_does_not_scan_catalog_on_healthy_zero_hit() {
        let store = CatalogStore::open_in_memory().unwrap();
        let objects = (0..60)
            .map(|i| table(&format!("object_{i}"), &["id"], &[]))
            .collect::<Vec<_>>();
        sync_all(&store, &objects);

        let ranked = search_store_objects(
            &store,
            "needle_that_does_not_exist",
            10,
            SearchFilter::default(),
        )
        .unwrap();

        assert!(ranked.is_empty());
    }

    #[tokio::test]
    async fn hybrid_falls_back_to_fts_without_embeddings() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("users", &["id", "email"], &[]),
                table("logs", &["id", "message"], &[]),
            ],
        );
        let provider: crate::catalog::embed::DynEmbeddingProvider =
            std::sync::Arc::new(crate::catalog::embed::StubEmbeddingProvider::new());
        // No embeddings built yet → hybrid must equal the FTS-only result.
        let fts = search_store_objects(&store, "email", 10, SearchFilter::default()).unwrap();
        let hybrid = search_store_objects_hybrid(
            &store,
            "email",
            10,
            SearchFilter::default(),
            Some(&provider),
            None,
        )
        .await
        .unwrap();
        let fts_keys: Vec<&str> = fts.iter().map(|r| r.object.object_key.as_str()).collect();
        let hybrid_keys: Vec<&str> = hybrid
            .iter()
            .map(|r| r.object.object_key.as_str())
            .collect();
        assert_eq!(fts_keys, hybrid_keys);
    }

    #[tokio::test]
    async fn hybrid_runs_vector_path_after_building_embeddings() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("users", &["id", "email"], &[]),
                table("logs", &["id", "message"], &[]),
            ],
        );
        let provider: crate::catalog::embed::DynEmbeddingProvider =
            std::sync::Arc::new(crate::catalog::embed::StubEmbeddingProvider::new());

        // Build embeddings for the active provider, then confirm the vector path
        // is actually exercised (live embeddings exist for this provider).
        let report = crate::catalog::vector::build_embedding_cache(
            &store,
            &provider,
            crate::catalog::vector::EmbeddingBuildOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert!(report.embedded >= 2, "expected embeddings to be built");
        assert!(
            store
                .live_embedding_count(crate::catalog::embed::STUB_PROVIDER_ID)
                .unwrap()
                >= 2
        );

        // Hybrid search succeeds and still surfaces the FTS-relevant object.
        let hybrid = search_store_objects_hybrid(
            &store,
            "email",
            10,
            SearchFilter::default(),
            Some(&provider),
            None,
        )
        .await
        .unwrap();
        assert!(
            hybrid.iter().any(|r| r.object.name == "users"),
            "expected users to appear in hybrid results"
        );
    }

    #[tokio::test]
    async fn hybrid_ignores_embeddings_from_a_different_provider() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(&store, &[table("users", &["id", "email"], &[])]);

        // Persist an embedding under a DIFFERENT provider id than the active one.
        store
            .upsert_embedding("dbo.users", "h1", "some-other-provider", &[0.1, 0.2])
            .unwrap();

        let provider: crate::catalog::embed::DynEmbeddingProvider =
            std::sync::Arc::new(crate::catalog::embed::StubEmbeddingProvider::new());
        // The active stub provider has zero live embeddings, so hybrid must fall
        // back to FTS-only rather than running a no-op vector pass.
        let fts = search_store_objects(&store, "email", 10, SearchFilter::default()).unwrap();
        let hybrid = search_store_objects_hybrid(
            &store,
            "email",
            10,
            SearchFilter::default(),
            Some(&provider),
            None,
        )
        .await
        .unwrap();
        let fts_keys: Vec<&str> = fts.iter().map(|r| r.object.object_key.as_str()).collect();
        let hybrid_keys: Vec<&str> = hybrid
            .iter()
            .map(|r| r.object.object_key.as_str())
            .collect();
        assert_eq!(fts_keys, hybrid_keys);
    }

    #[tokio::test]
    async fn hybrid_with_vector_store_uses_store_path() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_all(
            &store,
            &[
                table("users", &["id", "email"], &[]),
                table("logs", &["id", "message"], &[]),
            ],
        );
        let provider: crate::catalog::embed::DynEmbeddingProvider =
            std::sync::Arc::new(crate::catalog::embed::StubEmbeddingProvider::new());
        let store = std::sync::Arc::new(store);
        let vector_store: crate::catalog::vector_store::DynVectorStore = std::sync::Arc::new(
            crate::catalog::sqlite_vector_store::SqliteVectorStore::with_arc_provider(
                std::sync::Arc::clone(&store),
                crate::catalog::embed::STUB_PROVIDER_ID,
            ),
        );

        // Build embeddings through the vector store.
        let report = crate::catalog::vector::build_embedding_cache(
            &store,
            &provider,
            crate::catalog::vector::EmbeddingBuildOptions::default(),
            Some(&vector_store),
        )
        .await
        .unwrap();
        assert!(
            report.embedded >= 2,
            "expected embeddings to be built via vector store"
        );

        // Hybrid search with vector store should find the same results.
        let hybrid = search_store_objects_hybrid(
            &store,
            "email",
            10,
            SearchFilter::default(),
            Some(&provider),
            Some(&vector_store),
        )
        .await
        .unwrap();
        assert!(
            hybrid.iter().any(|r| r.object.name == "users"),
            "expected users to appear in hybrid results via vector store"
        );
    }
}
