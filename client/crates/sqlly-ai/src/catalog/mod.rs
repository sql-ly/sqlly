//! sqlly-catalog: persistent schema catalog + AI-assisted schema Q&A.
//!
//! The catalog persists schema facts observed from a connection (or DDL files)
//! in a local SQLite store keyed by connection + database identity, so state
//! survives across sessions. It detects changes via a timestamp gate backed by
//! a stable per-object hash, and maintains an opt-in "AI cache" of prebuilt
//! object cards invalidated automatically whenever an object changes.
//!
//! Typical flow:
//! 1. [`store::CatalogStore::sync`] — hand in freshly-observed objects; the
//!    store diffs them against persisted state and invalidates AI caches for
//!    anything that changed.
//! 2. [`ask::build_ai_cache`] — opt-in: precompute a card per stale object.
//! 3. [`ask::ask`] — answer a natural-language question against the catalog.

#![forbid(unsafe_code)]
pub mod agent;
pub mod agent_cli;
pub mod ask;
pub mod card;
pub mod decision;
pub mod documents;
pub mod embed;
pub mod fts;
pub mod graph;
pub mod hash;
pub mod hybrid;
pub mod infer;
pub mod infer_joins;
pub mod intent;
pub mod llm;
pub mod prompt;
pub mod review;
pub mod search;
pub mod sql_generation;
pub mod sqlite_vector_store;
pub mod store;
mod text;
mod tokens;
pub mod validation;
pub mod vector;
pub mod vector_store;
pub mod wire;

pub use agent::run::{AgentConfig, AgentEvent, AgentOutcome, CancelFlag, StopReason, run_agent};
pub use agent::tools::{
    AgentTool, AgentToolHost, MAX_CELL_CHARS, MAX_QUERY_ROWS, MAX_SAMPLE_ROWS,
    MAX_TOOL_RESULT_CHARS, dispatch as dispatch_agent_tool, parse_tool_call, tool_specs,
    window_text as window_tool_text,
};
pub use agent_cli::{
    CliAgentEvent, CliAgentKind, LaunchSpec, MAX_EVENT_BYTES, MAX_TOTAL_OUTPUT_BYTES,
    McpServerSpec, SCRUBBED_ENV_KEYS, SCRUBBED_ENV_PREFIXES, build_launch, cap_utf8,
    cli_agent_egress_label, parse_claude_line, parse_codex_line, parse_events_for_kind,
    toml_string, validate_env_key,
};
pub use ask::{
    BuildReport, DEFAULT_CONTEXT_TOKENS, ai_cache_status, ask, ask_with_objects,
    ask_with_objects_and_purpose, ask_with_purpose, ask_with_purpose_focused, build_ai_cache,
};
pub use card::{
    AssembledContext, CARD_FORMAT_VERSION, CardType, GeneratedObjectCard, ObjectCard,
    SchemaPurpose, assemble_context, assemble_context_for_purpose, assemble_context_from_store,
    compact_card, estimate_tokens, generate_metadata_cards,
};
pub use decision::{
    Answer, DecisionError, DecisionProvider, DecisionRequest, DecisionResponse,
    DynDecisionProvider, LLM_FALLBACK_PROVIDER_ID, LlmDecisionProvider, Question,
    STUB_DECISION_PROVIDER_ID, StubDecisionProvider,
};
pub use documents::{DOCUMENT_GENERATOR_VERSION, SearchDocument, generate_document};
pub use embed::{
    DynEmbeddingProvider, EmbeddingError, EmbeddingProvider, STUB_PROVIDER_ID,
    StubEmbeddingProvider,
};
pub use graph::{
    ExpandedNode, ExpansionResult, GraphExpansionOptions, JoinDirection, JoinPath, JoinPathOptions,
    JoinPathStep, expand_schema_graph, find_join_paths, find_join_paths_with_options,
};
pub use hash::object_hash;
pub use hybrid::{HybridCandidate, HybridSearchOptions, merge_hybrid};
pub use infer::{
    CandidateTarget, CandidateTargetIndex, InferenceColumn, InferenceObject, InferenceOptions,
    InferredEdge, NAME_INFERRED_REFERENCE, infer_relationships,
};
pub use infer_joins::{
    InferredJoin, MAX_CANDIDATE_PAIRS, MAX_INFERRED_JOINS, MIN_JOIN_PROBABILITY, TableCols,
    candidate_pairs, infer_joins,
};
pub use intent::{SqlIntent, detect_intent, detect_sql_intent};
pub use llm::{
    ChatMessage, ChatRole, CompletionRequest, CompletionResponse, DynLlmProvider, LlmError,
    LlmPrompt, LlmProvider, LlmResponse, ModelCapability, ModelSpec, STUB_LLM_PROVIDER_ID,
    StubLlmProvider, ToolCall, ToolCompletion, ToolCompletionRequest, ToolMessage, ToolResult,
    ToolSpec,
};
pub use prompt::{
    GROUNDING_RULES, GroundedObject, SCHEMA_DATA_BEGIN, SCHEMA_DATA_END, SQL_SAFETY_RULES,
    fence_schema_data, grounded_objects, purpose_preamble, render_system, wrap_llm_schema_context,
    wrap_llm_user_question,
};
pub use review::{QuerySummaryReport, SqlReviewReport, review_sql, summarize_query};
pub use search::{
    RankedObject, rank_for_question, relevant_only, search_store_objects,
    search_store_objects_hybrid,
};
pub use sql_generation::{
    AttemptCompletion, DEFAULT_SQL_GENERATION_ATTEMPTS, MAX_SQL_GENERATION_ATTEMPTS,
    SqlGenerationAttempt, SqlGenerationOptions, SqlGenerationResult, extract_sql, generate_sql,
};
pub use sqlite_vector_store::SqliteVectorStore;
pub use store::{
    AiCacheStatus, CatalogError, CatalogResult, CatalogScope, CatalogStore, MaintenanceOptions,
    MaintenanceReport, NeighborDirection, NeighborEdge, NukeAiDataReport, ObjectChange,
    SchemaSearchHit, SearchFilter, StoredObject, StoredObjectCard, StoredRelationship,
    SyncCoverage, SyncReport,
};
pub use validation::{
    ReferencedColumn, SqlIssueCode, SqlIssueSeverity, SqlValidationIssue, SqlValidationOptions,
    SqlValidationResult, validate_sql,
};
pub use vector::{
    EmbeddingBuildOptions, EmbeddingBuildReport, EmbeddingDocument, StoredEmbedding,
    VectorSearchResult, build_embedding_cache, cosine_similarity, decode_embedding, embedding_text,
    encode_embedding, vector_search,
};
pub use vector_store::{DynVectorStore, VectorFilter, VectorHit, VectorItem, VectorStore};
pub use wire::{
    CatalogColumn, CatalogForeignKey, CatalogObject, CatalogObjectKind, CatalogParameter,
    CatalogSource, SourceKind,
};
