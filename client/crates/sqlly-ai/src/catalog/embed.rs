//! Embedding provider abstraction.
//!
//! The catalog never calls a specific embedding model directly; it talks through
//! [`EmbeddingProvider`]. A deterministic hash-based stub ships here so the whole
//! vector search flow is testable end-to-end without an external service. Real
//! backends implement the same trait: `sqlly-ollama` and `sqlly-mlx` provide
//! them, selected via `SQLLY_EMBEDDING_PROVIDER` (see
//! `sqlly_services::providers`).

use std::sync::Arc;

use async_trait::async_trait;

use crate::catalog::llm::ModelSpec;

/// The contract for anything that can produce vector embeddings.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text string.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed a batch of text strings. Default implementation calls `embed` per
    /// item; real providers should override this to amortize model calls.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await?);
        }
        Ok(out)
    }

    /// New primary method: batch-aware, model-aware embedding.
    ///
    /// Default implementation delegates to `embed_batch` for backward
    /// compatibility during migration.
    async fn embed_batch_model(
        &self,
        input: &[String],
        _model: Option<&str>,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.embed_batch(input).await
    }

    /// The number of dimensions this provider outputs for the default model.
    fn dims(&self) -> usize;

    /// Dimensions for a specific model, if the provider supports model selection.
    fn dims_for(&self, _model: Option<&str>) -> Option<usize> {
        Some(self.dims())
    }

    /// A stable identifier for this provider, used to detect stale embeddings
    /// when the provider changes (e.g. from stub to a real model).
    fn provider_id(&self) -> &str;

    /// Models advertised by this provider. Default is empty.
    fn models(&self) -> &[ModelSpec] {
        &[]
    }

    /// Convenience: embed a single text with optional model selection.
    async fn embed_one(&self, text: &str, model: Option<&str>) -> Result<Vec<f32>, EmbeddingError> {
        let vectors = self.embed_batch_model(&[text.to_owned()], model).await?;
        vectors.into_iter().next().ok_or_else(|| {
            EmbeddingError::Other("embedding provider returned no vectors".to_string())
        })
    }
}

/// Failures from an embedding provider.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding provider error: {0}")]
    Other(String),

    #[error(
        "embedding dimension mismatch: model {model_id} expected {expected} dims but got {actual}"
    )]
    DimensionMismatch {
        model_id: String,
        expected: usize,
        actual: usize,
    },
}

impl EmbeddingError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        match self {
            Self::Other(detail) => sqlly_lang::DisplayMessage::raw(detail),
            Self::DimensionMismatch {
                model_id,
                expected,
                actual,
            } => sqlly_lang::DisplayMessage::new(
                "services.ai.embedding.dimension_mismatch",
                self.to_string(),
            )
            .with_arg("model", model_id)
            .with_arg("expected", expected.to_string())
            .with_arg("actual", actual.to_string()),
        }
    }
}

/// A type-erased provider handle convenient for storage in engine state.
pub type DynEmbeddingProvider = Arc<dyn EmbeddingProvider>;

/// Provider identifier for the built-in stub.
pub const STUB_PROVIDER_ID: &str = "stub-hash-v1";

/// Number of dimensions the stub provider emits.
pub const STUB_EMBEDDING_DIMS: usize = 64;

/// A deterministic, dependency-free embedding provider used for tests and as the
/// default when no real model is configured.
///
/// It does not produce semantic embeddings, but token-hash signed bucket
/// projection with L2 normalization. This gives deterministic vectors that
/// respond to token overlap — enough to verify the whole pipeline (build →
/// store → search → hybrid merge) works.
pub struct StubEmbeddingProvider;

impl StubEmbeddingProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubEmbeddingProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingProvider for StubEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(embed_stub(text))
    }

    fn dims(&self) -> usize {
        STUB_EMBEDDING_DIMS
    }

    fn provider_id(&self) -> &str {
        STUB_PROVIDER_ID
    }
}

/// Produce a deterministic 64-dim vector from text using token-hash signed
/// bucket projection with L2 normalization. Uses SHA-256 (already a crate
/// dependency) for each token to derive a stable bucket index and sign bit.
fn embed_stub(text: &str) -> Vec<f32> {
    use sha2::{Digest, Sha256};

    let mut vector = vec![0.0f32; STUB_EMBEDDING_DIMS];

    for token in crate::catalog::tokens::tokenize_terms(text) {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let digest = hasher.finalize();

        let bucket = (u64::from_le_bytes(digest[0..8].try_into().unwrap_or([0u8; 8]))
            % STUB_EMBEDDING_DIMS as u64) as usize;
        let sign_bit = digest[8] & 1;
        let sign = if sign_bit == 0 { 1.0f32 } else { -1.0f32 };
        vector[bucket] += sign;
    }

    l2_normalize(&mut vector);
    vector
}

pub(crate) fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_embedding_is_deterministic() {
        let provider = StubEmbeddingProvider::new();
        let a = provider.embed("hello world").await.unwrap();
        let b = provider.embed("hello world").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn stub_embedding_has_expected_dimension() {
        let provider = StubEmbeddingProvider::new();
        let v = provider.embed("test").await.unwrap();
        assert_eq!(v.len(), STUB_EMBEDDING_DIMS);
    }

    #[tokio::test]
    async fn stub_embedding_handles_empty_text() {
        let provider = StubEmbeddingProvider::new();
        let v = provider.embed("").await.unwrap();
        assert_eq!(v.len(), STUB_EMBEDDING_DIMS);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert_eq!(norm, 0.0);
    }

    #[tokio::test]
    async fn stub_embedding_batch_preserves_order() {
        let provider = StubEmbeddingProvider::new();
        let texts: Vec<String> = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let batch = provider.embed_batch(&texts).await.unwrap();
        assert_eq!(batch.len(), 3);
        let single_a = provider.embed("alpha").await.unwrap();
        let single_b = provider.embed("beta").await.unwrap();
        assert_eq!(batch[0], single_a);
        assert_eq!(batch[1], single_b);
    }

    #[tokio::test]
    async fn stub_embedding_related_terms_score_above_unrelated() {
        let provider = StubEmbeddingProvider::new();
        let query = provider.embed("customer order invoice").await.unwrap();
        let related = provider
            .embed("customer orders and invoices")
            .await
            .unwrap();
        let unrelated = provider.embed("weather forecast rain").await.unwrap();
        let score_rel = cosine_similarity(&query, &related);
        let score_unrel = cosine_similarity(&query, &unrelated);
        assert!(
            score_rel > score_unrel,
            "related cosine ({score_rel}) should exceed unrelated ({score_unrel})"
        );
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }
}
