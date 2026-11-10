//! sqlly-mlx: MLX sidecar HTTP client and provider implementations.
//!
//! This crate talks to an MLX sidecar process via OpenAI-compatible HTTP
//! endpoints. The sidecar handles model discovery, loading, chat completions,
//! and embeddings. The Rust side only knows about HTTP contracts, not MLX
//! internals.

#![forbid(unsafe_code)]

mod client;
mod config;
mod embed;
mod error;
mod llm;

pub use client::{MlxModelInfo, MlxSidecarClient, MlxSidecarHealth};
pub use config::MlxSidecarConfig;
pub use config::{
    BASE_URL_ENV, CHAT_MODEL_ENV, CONNECT_TIMEOUT_MS_ENV, DEFAULT_BASE_URL,
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_MAX_EMBEDDING_BATCH, DEFAULT_TIMEOUT_SECS,
    EMBEDDING_MODEL_ENV, MAX_EMBEDDING_BATCH_ENV, TIMEOUT_SECS_ENV,
};
pub use embed::MlxSidecarEmbeddingProvider;
pub use error::MlxError;
pub use llm::MlxSidecarLlmProvider;
