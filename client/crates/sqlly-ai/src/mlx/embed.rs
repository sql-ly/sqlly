//! [`MlxSidecarEmbeddingProvider`]: a `sqlly-catalog` [`EmbeddingProvider`]
//! backed by the MLX sidecar.

use crate::catalog::{EmbeddingError, EmbeddingProvider};
use async_trait::async_trait;

use crate::mlx::client::MlxSidecarClient;
use crate::mlx::config::MlxSidecarConfig;
use crate::mlx::error::MlxError;

/// An [`EmbeddingProvider`] that produces vectors via the MLX sidecar.
#[derive(Debug)]
pub struct MlxSidecarEmbeddingProvider {
    client: MlxSidecarClient,
    model: String,
    dims: usize,
    provider_id: String,
}

impl MlxSidecarEmbeddingProvider {
    pub async fn connect(
        config: &MlxSidecarConfig,
        model: impl Into<String>,
    ) -> Result<Self, MlxError> {
        let model = model.into();
        let client = MlxSidecarClient::new(config)?;
        let dims = Self::probe_dims(&client, &model).await?;
        Ok(Self::from_parts(client, model, dims))
    }

    pub fn with_client(client: MlxSidecarClient, model: impl Into<String>, dims: usize) -> Self {
        Self::from_parts(client, model.into(), dims)
    }

    async fn probe_dims(client: &MlxSidecarClient, model: &str) -> Result<usize, MlxError> {
        let resp = client
            .embed_batch(model, vec!["schema".to_string()])
            .await?;
        let first =
            resp.data.into_iter().next().ok_or_else(|| {
                MlxError::Empty(format!("model '{model}' returned no embeddings"))
            })?;
        if first.embedding.is_empty() {
            return Err(MlxError::Empty(format!(
                "model '{model}' returned an empty embedding vector"
            )));
        }
        Ok(first.embedding.len())
    }

    fn from_parts(client: MlxSidecarClient, model: String, dims: usize) -> Self {
        let provider_id = format!("mlx:{model}");
        Self {
            client,
            model,
            dims,
            provider_id,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl EmbeddingProvider for MlxSidecarEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.client
            .embed_batch(&self.model, vec![text.to_string()])
            .await
            .map_err(|err| EmbeddingError::Other(err.to_string()))?
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| EmbeddingError::Other("MLX sidecar returned no embedding".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let resp = self
            .client
            .embed_batch(&self.model, texts.to_vec())
            .await
            .map_err(|err| EmbeddingError::Other(err.to_string()))?;
        let mut out = Vec::with_capacity(texts.len());
        for d in resp.data {
            out.push(d.embedding);
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
                "MlxSidecarEmbeddingProvider was constructed for model '{}' but asked to embed with '{}'",
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
