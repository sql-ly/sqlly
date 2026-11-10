//! [`OllamaEmbeddingProvider`]: a `sqlly-catalog` [`EmbeddingProvider`] backed
//! by a local Ollama embedding model.

use crate::catalog::{EmbeddingError, EmbeddingProvider};
use async_trait::async_trait;

use crate::ollama::client::{OllamaClient, OllamaConfig, OllamaError};

/// The text used to probe a model's embedding dimensionality at construction.
const DIMS_PROBE_TEXT: &str = "schema";

/// Maximum number of inputs sent to `/api/embed` in a single request.
///
/// `/api/embed` batches server-side, so this collapses N texts into
/// `ceil(N / EMBED_BATCH_SIZE)` round trips instead of N. The cap stays modest
/// on purpose: a local model on Apple Silicon is memory-bound, so we trade a
/// little batching for bounded peak memory rather than sending an unbounded
/// payload.
const EMBED_BATCH_SIZE: usize = 16;

/// An [`EmbeddingProvider`] that produces vectors via a local Ollama model.
///
/// Unlike the LLM provider, this one performs a one-shot probe at construction
/// to discover the model's output dimensionality (the [`EmbeddingProvider`]
/// trait requires a synchronous `dims()`). That probe doubles as a health
/// check: if Ollama is not running or the model is not installed, construction
/// fails and the caller falls back to the deterministic stub. The probe uses a
/// short timeout (see [`OllamaConfig::probe_timeout`]) so a dead runtime cannot
/// stall engine startup.
#[derive(Debug)]
pub struct OllamaEmbeddingProvider {
    client: OllamaClient,
    model: String,
    dims: usize,
    provider_id: String,
}

impl OllamaEmbeddingProvider {
    /// Connect from config + model name, probing for dimensionality. Returns an
    /// error (so the caller can fall back to a stub) if the runtime is
    /// unreachable, the model is missing, or it returns an empty vector.
    ///
    /// The probe runs on a separate client bound to the *short*
    /// [`OllamaConfig::probe_timeout`], while ongoing embeds use the normal
    /// [`OllamaConfig::timeout`].
    pub async fn connect(
        config: &OllamaConfig,
        model: impl Into<String>,
    ) -> Result<Self, OllamaError> {
        let model = model.into();
        let probe_client = OllamaClient::with_timeout(config, config.probe_timeout)?;
        let dims = Self::probe_dims(&probe_client, &model).await?;
        let client = OllamaClient::new(config)?;
        Ok(Self::from_parts(client, model, dims))
    }

    /// Connect using an existing client (used by tests and callers that share a
    /// client). The same client is used for the probe and ongoing embeds.
    pub async fn connect_with_client(
        client: OllamaClient,
        model: impl Into<String>,
    ) -> Result<Self, OllamaError> {
        let model = model.into();
        let dims = Self::probe_dims(&client, &model).await?;
        Ok(Self::from_parts(client, model, dims))
    }

    /// Probe `model` for its embedding dimensionality, erroring if it returns an
    /// empty vector (a strong signal the model is not an embedding model).
    async fn probe_dims(client: &OllamaClient, model: &str) -> Result<usize, OllamaError> {
        let probe = client.embed(model, DIMS_PROBE_TEXT).await?;
        if probe.is_empty() {
            return Err(OllamaError::Empty(format!(
                "embedding model '{model}' returned an empty vector; is it an embedding model?"
            )));
        }
        Ok(probe.len())
    }

    fn from_parts(client: OllamaClient, model: String, dims: usize) -> Self {
        let provider_id = format!("ollama:{model}");
        Self {
            client,
            model,
            dims,
            provider_id,
        }
    }

    /// The model name this provider targets.
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.client
            .embed(&self.model, text)
            .await
            .map_err(|err| EmbeddingError::Other(err.to_string()))
    }

    /// Override the per-item default with bounded server-side batching: inputs
    /// are sent in chunks of [`EMBED_BATCH_SIZE`] to `/api/embed`, preserving
    /// order, so a large `build_embeddings` run makes far fewer round trips
    /// without overwhelming a local model with an unbounded payload.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(EMBED_BATCH_SIZE) {
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let vectors = self
                .client
                .embed_batch(&self.model, &refs)
                .await
                .map_err(|err| EmbeddingError::Other(err.to_string()))?;
            if vectors.len() != chunk.len() {
                return Err(EmbeddingError::Other(format!(
                    "Ollama returned {} vectors for {} inputs from embedding model '{}'",
                    vectors.len(),
                    chunk.len(),
                    self.model
                )));
            }
            out.extend(vectors);
        }
        Ok(out)
    }

    async fn embed_batch_model(
        &self,
        input: &[String],
        model: Option<&str>,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let target = model.unwrap_or(&self.model);
        if target != self.model {
            return Err(EmbeddingError::Other(format!(
                "OllamaEmbeddingProvider was constructed for model '{}' but asked to embed with '{}'",
                self.model, target
            )));
        }
        self.embed_batch(input).await
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn dims_for(&self, model: Option<&str>) -> Option<usize> {
        if model.is_none() || model == Some(&self.model) {
            Some(self.dims)
        } else {
            None
        }
    }

    fn provider_id(&self) -> &str {
        &self.provider_id
    }
}
