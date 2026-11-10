//! The AI side of SQLLY: the searchable catalog of database objects that
//! grounds every answer, the agent/tool vocabulary, and the model providers
//! that run the prompts.
//!
//! `catalog` is the store — documents, embeddings, hybrid search, prompt
//! assembly, generated-SQL validation. `agent` is the conversation and tool
//! model. `ollama` and `mlx` are the providers, native-only because one talks
//! to a local daemon and the other to an on-device runtime.

pub mod agent;
pub mod catalog;

#[cfg(not(target_arch = "wasm32"))]
pub mod jev;
#[cfg(not(target_arch = "wasm32"))]
pub mod mlx;
#[cfg(not(target_arch = "wasm32"))]
pub mod ollama;
