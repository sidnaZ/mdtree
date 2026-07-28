//! Provider-independent semantic indexing and search contracts.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Node, NodeHash, NodeId, Page, SearchMatch, Section};

/// Current version of the exact text representation sent to embedding models.
pub const SEMANTIC_INPUT_FORMAT_VERSION: u32 = 1;

/// Search channel requested by a caller.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Existing deterministic FTS-backed search.
    #[default]
    Lexical,
    /// Embedding similarity without lexical rank fusion.
    Semantic,
    /// Deterministic fusion of lexical and semantic rankings.
    Hybrid,
}

impl SearchMode {
    /// Returns whether this mode preserves the compatibility-default lexical behavior.
    #[must_use]
    pub const fn is_lexical(&self) -> bool {
        matches!(self, Self::Lexical)
    }
}

/// Similarity metric associated with one compatible embedding profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingMetric {
    /// Cosine similarity between vectors.
    Cosine,
}

impl EmbeddingMetric {
    /// Stable string included in profile and input hashes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
        }
    }
}

/// Complete identity required before two embeddings may be compared.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingProfile {
    /// Provider implementation, such as `ollama`.
    pub provider: String,
    /// Provider-specific model name.
    pub model: String,
    /// Exact vector length returned by the model.
    pub dimensions: u32,
    /// Similarity metric used for retrieval.
    pub metric: EmbeddingMetric,
    /// Version of the exact formatted text embedded by the model.
    pub input_format_version: u32,
}

/// One bounded, model-input-ready piece of a Markdown section.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticChunk {
    /// Canonical node containing the source section.
    pub node_id: NodeId,
    /// Derived section containing this chunk.
    pub section_id: NodeId,
    /// Zero-based order within the source section.
    pub position: u32,
    /// Inclusive byte offset in the canonical node Markdown.
    pub start_byte: u64,
    /// Exclusive byte offset in the canonical node Markdown.
    pub end_byte: u64,
    /// Exact versioned text sent to the embedding provider.
    pub input: String,
    /// Hash of the complete profile identity and exact input bytes.
    pub input_hash: NodeHash,
}

/// Canonical node and its current derived sections used to prepare chunks.
#[derive(Clone, Debug)]
pub struct SemanticSource {
    /// Canonical node whose metadata and Markdown form embedding inputs.
    pub node: Node,
    /// Current stable derived sections in document order.
    pub sections: Vec<Section>,
}

/// Persisted lifecycle state of one semantic chunk.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticChunkState {
    /// Waiting for an embedding worker.
    Pending,
    /// Claimed by one embedding worker.
    Processing,
    /// Contains a validated compatible vector.
    Ready,
    /// The most recent embedding attempt failed.
    Failed,
}

/// One claimed unit of embedding work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticChunkWork {
    /// Canonical node containing the chunk.
    pub node_id: NodeId,
    /// Derived section containing the chunk.
    pub section_id: NodeId,
    /// Zero-based chunk order within the section.
    pub position: u32,
    /// Exact formatted provider input.
    pub input: String,
    /// Optimistic freshness token.
    pub input_hash: NodeHash,
    /// Attempt number after this claim.
    pub attempt: u32,
}

/// Outcome of conditionally storing embedding work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticWriteOutcome {
    /// The current chunk accepted the result.
    Stored,
    /// The chunk or its input/profile changed before completion.
    Stale,
}

/// Aggregate lifecycle state of the active semantic index.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticIndexState {
    /// No semantic chunks are currently registered.
    Empty,
    /// Chunks exist but none is ready.
    Pending,
    /// Some, but not all, chunks are ready.
    Partial,
    /// Every registered chunk is ready.
    Ready,
    /// Every outstanding chunk has failed.
    Failed,
}

/// Exact semantic-index lifecycle counts and independent revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticIndexCoverage {
    /// Total chunks registered for the active profile.
    pub total: u64,
    /// Chunks waiting for embedding work.
    pub pending: u64,
    /// Chunks currently claimed by an indexer.
    pub processing: u64,
    /// Chunks with compatible validated vectors.
    pub ready: u64,
    /// Chunks whose latest embedding attempt failed.
    pub failed: u64,
    /// Monotonic revision changed by semantic-index mutations.
    pub revision: u64,
}

impl SemanticIndexCoverage {
    /// Derives a stable aggregate state from exact lifecycle counts.
    #[must_use]
    pub const fn state(&self) -> SemanticIndexState {
        if self.total == 0 {
            SemanticIndexState::Empty
        } else if self.ready == self.total {
            SemanticIndexState::Ready
        } else if self.ready > 0 {
            SemanticIndexState::Partial
        } else if self.failed == self.total {
            SemanticIndexState::Failed
        } else {
            SemanticIndexState::Pending
        }
    }
}

/// Provider-independent semantic-index status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticIndexStatus {
    /// Active compatible profile, absent before semantic indexing is configured.
    pub profile: Option<EmbeddingProfile>,
    /// Exact lifecycle coverage.
    pub coverage: SemanticIndexCoverage,
}

/// One exact semantic-search page plus its index compatibility evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SemanticSearchResponse {
    /// Ranked, section-aware matches and opaque continuation.
    pub matches: Page<SearchMatch>,
    /// Active profile and complete coverage used for the query.
    pub index: SemanticIndexStatus,
    /// Compatible eligible ready chunk vectors compared for this query.
    pub scanned_chunks: u64,
}

/// One deterministic hybrid-search page and channel-state evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HybridSearchResponse {
    /// Reciprocal-rank-fused matches and opaque continuation.
    pub matches: Page<SearchMatch>,
    /// Semantic profile and coverage observed during fusion.
    pub index: SemanticIndexStatus,
    /// Compatible eligible semantic chunks compared before fusion.
    pub scanned_chunks: u64,
    /// Explicit semantic failure category when lexical fallback was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<SemanticErrorCode>,
}

/// Stable machine-readable semantic operation failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticErrorCode {
    /// No provider/model profile was configured.
    NotConfigured,
    /// The configured embedding provider could not be reached.
    ProviderUnavailable,
    /// The configured embedding provider exceeded the request deadline.
    Timeout,
    /// The configured model does not exist or cannot produce embeddings.
    ModelUnavailable,
    /// A bounded input still exceeds the provider's accepted context.
    InputTooLarge,
    /// The provider returned malformed or incompatible data.
    InvalidResponse,
    /// Stored and requested profile identities are incompatible.
    IncompatibleProfile,
    /// The semantic index does not yet cover every current chunk.
    PartialIndex,
    /// Work completed for an input that is no longer current.
    StaleWork,
    /// Another provider or persistence operation failed.
    OperationFailed,
}

/// Provider-independent semantic operation error with a stable category.
#[derive(Clone, Debug, Deserialize, Eq, Error, PartialEq, Serialize)]
#[error("{message}")]
pub struct SemanticError {
    /// Stable machine-readable category.
    pub code: SemanticErrorCode,
    /// Human-readable detail that must not contain indexed document content.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        EmbeddingMetric, EmbeddingProfile, SearchMode, SemanticErrorCode, SemanticIndexCoverage,
        SemanticIndexState, SemanticIndexStatus, SEMANTIC_INPUT_FORMAT_VERSION,
    };

    #[test]
    fn contracts_have_stable_json_and_lexical_default() {
        assert_eq!(
            serde_json::to_value(SearchMode::default()).expect("mode"),
            json!("lexical")
        );
        let status = SemanticIndexStatus {
            profile: Some(EmbeddingProfile {
                provider: "ollama".into(),
                model: "embeddinggemma".into(),
                dimensions: 768,
                metric: EmbeddingMetric::Cosine,
                input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
            }),
            coverage: SemanticIndexCoverage {
                total: 4,
                pending: 1,
                processing: 0,
                ready: 2,
                failed: 1,
                revision: 7,
            },
        };
        assert_eq!(
            serde_json::to_value(status).expect("status"),
            json!({
                "profile": {
                    "provider": "ollama",
                    "model": "embeddinggemma",
                    "dimensions": 768,
                    "metric": "cosine",
                    "input_format_version": 1
                },
                "coverage": {
                    "total": 4,
                    "pending": 1,
                    "processing": 0,
                    "ready": 2,
                    "failed": 1,
                    "revision": 7
                }
            })
        );
        assert_eq!(SemanticErrorCode::StaleWork, SemanticErrorCode::StaleWork);
    }

    #[test]
    fn coverage_state_is_derived_without_ambiguity() {
        let coverage = |total, pending, processing, ready, failed| SemanticIndexCoverage {
            total,
            pending,
            processing,
            ready,
            failed,
            revision: 1,
        };
        assert_eq!(coverage(0, 0, 0, 0, 0).state(), SemanticIndexState::Empty);
        assert_eq!(coverage(2, 2, 0, 0, 0).state(), SemanticIndexState::Pending);
        assert_eq!(coverage(2, 0, 0, 1, 1).state(), SemanticIndexState::Partial);
        assert_eq!(coverage(2, 0, 0, 2, 0).state(), SemanticIndexState::Ready);
        assert_eq!(coverage(2, 0, 0, 0, 2).state(), SemanticIndexState::Failed);
    }
}
