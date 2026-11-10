//! Hybrid search: merge FTS and vector search results with normalized scoring.
//!
//! Raw FTS (BM25) scores and vector cosine similarity are not comparable.
//! This module normalizes each score set independently, then blends them with
//! configurable weights. The existing FTS-only path is preserved untouched;
//! hybrid is only engaged when embeddings exist.

use std::collections::HashMap;

use crate::catalog::search::RankedObject;
use crate::catalog::store::StoredObject;
use crate::catalog::vector::VectorSearchResult;
use crate::catalog::wire::{CatalogObjectKind, SourceKind};

/// Default weights for hybrid scoring.
pub const DEFAULT_FTS_WEIGHT: f64 = 0.45;
pub const DEFAULT_VECTOR_WEIGHT: f64 = 0.45;
pub const DEFAULT_BOOST_WEIGHT: f64 = 0.10;

/// Configuration for hybrid score merging.
#[derive(Debug, Clone)]
pub struct HybridSearchOptions {
    pub fts_weight: f64,
    pub vector_weight: f64,
    pub boost_weight: f64,
}

impl Default for HybridSearchOptions {
    fn default() -> Self {
        Self {
            fts_weight: DEFAULT_FTS_WEIGHT,
            vector_weight: DEFAULT_VECTOR_WEIGHT,
            boost_weight: DEFAULT_BOOST_WEIGHT,
        }
    }
}

/// A candidate produced by hybrid search, carrying score breakdown for
/// debugging and UI display.
#[derive(Debug, Clone)]
pub struct HybridCandidate {
    pub object_key: String,
    pub score: f64,
    pub fts_score: f64,
    pub vector_score: f64,
    pub object: StoredObject,
}

/// Merge FTS and vector search results using normalized score blending.
///
/// Normalization: each score set is min-max normalized to [0, 1] independently.
/// Singleton or empty sets receive a default of 1.0 for present hits. Vector
/// cosine scores are clamped to [0, 1] before blending (negative cosine treated
/// as 0). The `boost_weight` is reserved for existing search boosts and is
/// zero in this phase — it preserves the slot so future boosts are additive.
pub fn merge_hybrid(
    fts_hits: &[RankedObject],
    vector_hits: &[VectorSearchResult],
    options: &HybridSearchOptions,
) -> Vec<HybridCandidate> {
    use std::collections::HashMap;

    let norm_fts = normalize_scores(
        fts_hits
            .iter()
            .map(|h| (h.object.object_key.as_str(), h.score)),
    );
    let norm_vec = normalize_scores(
        vector_hits
            .iter()
            // Clamp cosine similarity ([-1, 1]) to [0, 1] before normalization:
            // a negative cosine means the query points away from the object, so
            // treat it as no vector contribution rather than a negative signal.
            .map(|h| (h.object_key.as_str(), (h.score as f64).clamp(0.0, 1.0))),
    );

    let all_keys: std::collections::HashSet<&str> =
        norm_fts.keys().chain(norm_vec.keys()).copied().collect();

    let fts_by_key: HashMap<&str, &RankedObject> = fts_hits
        .iter()
        .map(|h| (h.object.object_key.as_str(), h))
        .collect();

    let mut merged: Vec<HybridCandidate> = all_keys
        .into_iter()
        .map(|key| {
            let fts = norm_fts.get(key).copied().unwrap_or(0.0);
            let vec = norm_vec.get(key).copied().unwrap_or(0.0);

            let object = if let Some(ranked) = fts_by_key.get(key) {
                ranked.object.clone()
            } else {
                fts_hits
                    .iter()
                    .find(|h| h.object.object_key == key)
                    .map(|h| h.object.clone())
                    .unwrap_or_else(|| StoredObject {
                        object_key: key.to_string(),
                        catalog: None,
                        namespace: None,
                        name: key.to_string(),
                        kind: CatalogObjectKind::Table,
                        source_kind: SourceKind::Database,
                        source_path: None,
                        source_timestamp: None,
                        object_hash: String::new(),
                        body_json: String::new(),
                        ai_cache_valid: false,
                        ai_card_json: None,
                    })
            };

            let score =
                options.fts_weight * fts + options.vector_weight * vec + options.boost_weight * 0.0;

            HybridCandidate {
                object_key: key.to_string(),
                score,
                fts_score: fts,
                vector_score: vec,
                object,
            }
        })
        .collect();

    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.object.name.cmp(&b.object.name))
            .then_with(|| a.object_key.cmp(&b.object_key))
    });

    merged
}

/// Min-max normalize a set of (key, score) pairs to [0, 1]. Returns a HashMap
/// of key → normalized score. Singleton sets map the single entry to 1.0.
/// Empty inputs return an empty map.
fn normalize_scores<'a, I>(scores: I) -> HashMap<&'a str, f64>
where
    I: Iterator<Item = (&'a str, f64)>,
{
    let collected: Vec<(&'a str, f64)> = scores.collect();
    if collected.is_empty() {
        return HashMap::new();
    }

    let min = collected
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::INFINITY, f64::min);
    let max = collected
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);

    let range = max - min;
    collected
        .into_iter()
        .map(|(key, score)| {
            let normalized = if range == 0.0 {
                1.0
            } else {
                (score - min) / range
            };
            (key, normalized)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::StoredObject;
    use crate::catalog::wire::{CatalogObjectKind, SourceKind};

    fn stored_obj(key: &str, name: &str) -> StoredObject {
        StoredObject {
            object_key: key.to_string(),
            catalog: None,
            namespace: None,
            name: name.to_string(),
            kind: CatalogObjectKind::Table,
            source_kind: SourceKind::Database,
            source_path: None,
            source_timestamp: None,
            object_hash: String::new(),
            body_json: String::new(),
            ai_cache_valid: false,
            ai_card_json: None,
        }
    }

    #[test]
    fn merge_normalizes_fts_and_vector_scores() {
        let fts = vec![RankedObject {
            object: stored_obj("a", "alpha"),
            score: 10.0,
        }];
        let vec = vec![VectorSearchResult {
            object_key: "a".to_string(),
            score: 0.8,
        }];
        let merged = merge_hybrid(&fts, &vec, &HybridSearchOptions::default());
        assert_eq!(merged.len(), 1);
        assert!((merged[0].fts_score - 1.0).abs() < 1e-9);
        assert!((merged[0].vector_score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn merge_handles_empty_fts() {
        let vec = vec![VectorSearchResult {
            object_key: "a".to_string(),
            score: 0.9,
        }];
        let merged = merge_hybrid(&[], &vec, &HybridSearchOptions::default());
        assert_eq!(merged.len(), 1);
        assert!((merged[0].fts_score).abs() < 1e-9);
        assert!((merged[0].vector_score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn merge_handles_empty_vector() {
        let fts = vec![RankedObject {
            object: stored_obj("a", "alpha"),
            score: 5.0,
        }];
        let merged = merge_hybrid(&fts, &[], &HybridSearchOptions::default());
        assert_eq!(merged.len(), 1);
        assert!((merged[0].fts_score - 1.0).abs() < 1e-9);
        assert!((merged[0].vector_score).abs() < 1e-9);
    }

    #[test]
    fn merge_handles_singleton_score_sets() {
        let fts = vec![RankedObject {
            object: stored_obj("a", "alpha"),
            score: 7.0,
        }];
        let vec = vec![VectorSearchResult {
            object_key: "a".to_string(),
            score: 0.5,
        }];
        let merged = merge_hybrid(&fts, &vec, &HybridSearchOptions::default());
        assert_eq!(merged.len(), 1);
        assert!((merged[0].fts_score - 1.0).abs() < 1e-9);
        assert!((merged[0].vector_score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn merge_prefers_object_present_in_both() {
        let fts = vec![
            RankedObject {
                object: stored_obj("a", "alpha"),
                score: 10.0,
            },
            RankedObject {
                object: stored_obj("b", "beta"),
                score: 1.0,
            },
        ];
        let vec = vec![VectorSearchResult {
            object_key: "a".to_string(),
            score: 0.9,
        }];
        let merged = merge_hybrid(&fts, &vec, &HybridSearchOptions::default());
        assert_eq!(merged[0].object_key, "a");
        assert!(merged[0].score > merged[1].score);
    }

    #[test]
    fn merge_preserves_deterministic_tie_breaking() {
        let fts = vec![
            RankedObject {
                object: stored_obj("a", "same"),
                score: 5.0,
            },
            RankedObject {
                object: stored_obj("b", "same"),
                score: 5.0,
            },
        ];
        let merged = merge_hybrid(&fts, &[], &HybridSearchOptions::default());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].object_key, "a");
        assert_eq!(merged[1].object_key, "b");
    }

    #[test]
    fn merge_clamps_negative_cosine_to_zero_contribution() {
        // A negative cosine means the query points away from the object. After
        // clamping to [0, 1] the outlier (c) must not stretch the normalization
        // floor: b keeps the clamped normalized value (0.3/0.9 = 0.333…), not
        // the unclamped value that a -0.8 floor would produce ((0.3+0.8)/1.7 =
        // 0.647…).
        let vec = vec![
            VectorSearchResult {
                object_key: "a".to_string(),
                score: 0.9,
            },
            VectorSearchResult {
                object_key: "b".to_string(),
                score: 0.3,
            },
            VectorSearchResult {
                object_key: "c".to_string(),
                score: -0.8,
            },
        ];
        let merged = merge_hybrid(&[], &vec, &HybridSearchOptions::default());
        let by_key: std::collections::HashMap<&str, &HybridCandidate> =
            merged.iter().map(|c| (c.object_key.as_str(), c)).collect();

        // Negative cosine is treated as no contribution.
        assert!((by_key["c"].vector_score).abs() < 1e-9);
        // The small-positive hit reflects the clamped normalization, not the
        // inflated value a negative floor would produce.
        assert!(
            (by_key["b"].vector_score - (1.0 / 3.0)).abs() < 1e-6,
            "b vector_score was {}",
            by_key["b"].vector_score
        );
        assert!((by_key["a"].vector_score - 1.0).abs() < 1e-9);
    }
}
