//! Shared embedding-provider and semantic-index orchestration services.

mod hybrid;
mod indexer;
mod ollama;
mod provider;
mod search;

pub use hybrid::{
    fuse_rankings, search_hybrid, HybridFallbackPolicy, HybridSearchOptions,
    HYBRID_CANDIDATE_FACTOR, HYBRID_RRF_K, MAX_HYBRID_CANDIDATES,
};
pub use indexer::{
    build_semantic_index, clear_semantic_index, prepare_semantic_index, resume_semantic_index,
    retry_semantic_index, semantic_index_status, SemanticBuildFailure, SemanticBuildOptions,
    SemanticBuildReport,
};
pub use ollama::{OllamaConfig, OllamaProvider, DEFAULT_OLLAMA_BASE_URL};
pub use provider::EmbeddingProvider;
pub use search::{search_semantic, SemanticSearchError};
