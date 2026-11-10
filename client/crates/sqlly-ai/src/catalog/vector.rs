//! Vector storage, brute-force search, and the embedding cache builder.
//!
//! Embeddings are stored in the existing catalog SQLite as `f32` little-endian
//! blobs in the `catalog_embedding` table. Search is brute-force cosine
//! similarity computed in Rust over loaded blobs — no external vector library
//! needed. This keeps the Phase 4 dependency footprint at zero while the
//! architecture is ready for a future `sqlite-vec` or HNSW backend behind
//! [`VectorIndex`].

use crate::catalog::embed::DynEmbeddingProvider;
use crate::catalog::store::{CatalogError, CatalogStore};
use crate::catalog::vector_store::{DynVectorStore, VectorItem};

/// A stored embedding row read back from the catalog.
#[derive(Debug, Clone)]
pub struct StoredEmbedding {
    pub object_key: String,
    pub content_hash: String,
    pub dims: usize,
    pub embedding: Vec<f32>,
    pub provider_id: String,
}

/// A hit from vector similarity search.
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub object_key: String,
    pub score: f32,
}

/// Result of building the embedding cache.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct EmbeddingBuildReport {
    pub processed: usize,
    pub embedded: usize,
    pub failed: usize,
    pub remaining_stale: usize,
    pub dims: usize,
}

/// Options for the embedding cache builder.
#[derive(Debug, Clone)]
pub struct EmbeddingBuildOptions {
    pub limit: usize,
    pub batch_size: usize,
}

impl Default for EmbeddingBuildOptions {
    fn default() -> Self {
        Self {
            limit: 0,
            batch_size: 16,
        }
    }
}

/// Encode an `f32` slice as a little-endian byte blob for SQLite storage.
pub fn encode_embedding(vector: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(vector.len() * 4);
    for &val in vector {
        buf.extend_from_slice(&val.to_le_bytes());
    }
    buf
}

/// Decode a little-endian byte blob back into `f32` values.
// `as_chunks` (clippy 1.98's suggestion) needs Rust 1.88; the workspace MSRV
// is 1.85, so `chunks_exact` stays until the MSRV moves.
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
pub fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    if !blob.len().is_multiple_of(4) {
        return Vec::new();
    }
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Cosine similarity between two vectors. Returns 0.0 for zero-length or
/// dimension-mismatched inputs.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Brute-force vector search over stored embeddings. Reads all live embeddings
/// produced by `active_provider_id` from the store, computes cosine similarity
/// against the query vector, and returns the top `limit` hits sorted by score
/// descending, then key ascending.
pub fn vector_search(
    store: &CatalogStore,
    query: &[f32],
    limit: usize,
    active_provider_id: &str,
) -> Result<Vec<VectorSearchResult>, CatalogError> {
    let all = store.all_embeddings(active_provider_id)?;
    let mut hits: Vec<VectorSearchResult> = all
        .into_iter()
        .filter(|stored| stored.embedding.len() == query.len())
        .map(|stored| {
            let score = cosine_similarity(query, &stored.embedding);
            VectorSearchResult {
                object_key: stored.object_key,
                score,
            }
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.object_key.cmp(&b.object_key))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// Build the embedding cache: embed stale/missing search documents and persist
/// them. This is the only operation that writes to `catalog_embedding`. It must
/// NOT be called inside `sync` — sync only marks embeddings stale via
/// `content_hash` changes.
pub async fn build_embedding_cache(
    store: &CatalogStore,
    provider: &DynEmbeddingProvider,
    options: EmbeddingBuildOptions,
    vector_store: Option<&DynVectorStore>,
) -> Result<EmbeddingBuildReport, CatalogError> {
    let stale = store.stale_embedding_keys(options.limit, provider.provider_id())?;
    let expected_dims = provider.dims();
    let mut report = EmbeddingBuildReport {
        dims: expected_dims,
        ..Default::default()
    };
    report.processed = stale.len();

    if stale.is_empty() {
        report.remaining_stale = store.stale_embedding_count(provider.provider_id())?;
        return Ok(report);
    }

    for chunk in stale.chunks(options.batch_size) {
        let docs = store.embedding_documents_for_keys(chunk)?;
        let texts: Vec<String> = docs.iter().map(|d| d.text.clone()).collect();

        let embeddings = match provider.embed_batch(&texts).await {
            Ok(embeddings) => embeddings,
            Err(_) => {
                report.failed += chunk.len();
                continue;
            }
        };

        let mut items = Vec::new();
        for (doc, embedding) in docs.iter().zip(embeddings) {
            if embedding.len() != expected_dims {
                report.failed += 1;
                continue;
            }

            if let Some(_vs) = vector_store {
                items.push(VectorItem {
                    object_key: doc.object_key.clone(),
                    vector: embedding,
                    provider_id: provider.provider_id().to_string(),
                    model_id: provider.provider_id().to_string(),
                    dimensions: expected_dims,
                    content_hash: doc.content_hash.clone(),
                    source_type: "table".to_string(),
                    database: None,
                    schema: None,
                    table: None,
                    column: None,
                    object_type: None,
                    file_path: None,
                    modified_ts: None,
                    workspace_id: None,
                    project_id: None,
                    tenant_id: None,
                    metadata_json: String::new(),
                });
            } else {
                if store
                    .upsert_embedding(
                        &doc.object_key,
                        &doc.content_hash,
                        provider.provider_id(),
                        &embedding,
                    )
                    .is_err()
                {
                    report.failed += 1;
                    continue;
                }
                report.embedded += 1;
            }
        }

        if let Some(vs) = vector_store
            && !items.is_empty()
        {
            let count = items.len();
            match vs.upsert(items).await {
                Ok(()) => report.embedded += count,
                Err(_) => report.failed += count,
            }
        }
    }

    report.remaining_stale = store.stale_embedding_count(provider.provider_id())?;
    Ok(report)
}

/// A search document's text content for embedding input.
#[derive(Debug, Clone)]
pub struct EmbeddingDocument {
    pub object_key: String,
    pub content_hash: String,
    pub text: String,
}

/// Canonical text for embedding from a search document's fields. Must match
/// what `content_hash` in `catalog_search_document` covers (title + body +
/// keywords) so staleness detection is correct.
pub fn embedding_text(title: &str, body: &str, keywords: &str) -> String {
    format!("{title}\n{body}\n{keywords}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips() {
        let original = vec![1.0f32, -2.5, 0.0, 3.5];
        let blob = encode_embedding(&original);
        let decoded = decode_embedding(&blob);
        assert_eq!(decoded, original);
    }

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        let score = cosine_similarity(&v, &v);
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let score = cosine_similarity(&a, &b);
        assert!(score.abs() < 1e-6);
    }

    #[test]
    fn cosine_dimension_mismatch_returns_zero() {
        let a = vec![1.0f32, 2.0];
        let b = vec![1.0f32];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_empty_returns_zero() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn decode_empty_blob_returns_empty() {
        let decoded = decode_embedding(&[]);
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_non_aligned_blob_returns_empty() {
        let decoded = decode_embedding(&[1u8, 2, 3]);
        assert!(decoded.is_empty());
    }
}
