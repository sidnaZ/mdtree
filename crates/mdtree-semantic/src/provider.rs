//! Provider-independent embedding boundary.

use async_trait::async_trait;
use mdtree_core::{EmbeddingProfile, SemanticError};

/// Computes ordered embedding batches for one complete compatible profile.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embeds every input in order or rejects the complete batch.
    ///
    /// Implementations must return exactly one finite vector per input and
    /// validate every vector against `profile.dimensions`.
    async fn embed(
        &self,
        profile: &EmbeddingProfile,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, SemanticError>;
}
