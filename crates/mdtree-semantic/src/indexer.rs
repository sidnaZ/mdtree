//! Resumable semantic-index orchestration.

use mdtree_core::{
    EmbeddingProfile, SemanticError, SemanticErrorCode, SemanticIndexStatus, SemanticWriteOutcome,
};
use mdtree_markdown::{build_semantic_chunks, SemanticChunkOptions};
use mdtree_sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::EmbeddingProvider;

/// Bounded preparation and provider-call settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticBuildOptions {
    /// Maximum inputs sent in one provider request.
    pub batch_size: u32,
    /// Deterministic Markdown chunk bounds.
    pub chunks: SemanticChunkOptions,
}

impl Default for SemanticBuildOptions {
    fn default() -> Self {
        Self {
            batch_size: 32,
            chunks: SemanticChunkOptions::default(),
        }
    }
}

/// Observable progress from one build or resume attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SemanticBuildReport {
    /// Provider batches attempted.
    pub batches: u64,
    /// Embeddings accepted by the current index.
    pub stored: u64,
    /// Results rejected because their inputs changed during computation.
    pub stale: u64,
    /// Claimed chunks recorded as failed.
    pub failed: u64,
    /// Lifecycle status after the attempt.
    pub status: SemanticIndexStatus,
}

/// Stable failure plus the partial progress retained before it occurred.
#[derive(Clone, Debug, Error, PartialEq)]
#[error("{error}")]
pub struct SemanticBuildFailure {
    /// Provider-independent failure category and redacted detail.
    pub error: SemanticError,
    /// Durable progress retained before the failure.
    pub report: SemanticBuildReport,
}

/// Returns the current semantic profile and exact lifecycle coverage.
///
/// # Errors
///
/// Returns a redacted persistence error when status cannot be read.
pub fn semantic_index_status(store: &SqliteStore) -> Result<SemanticIndexStatus, SemanticError> {
    store.semantic_index_status().map_err(persistence_error)
}

/// Clears all derived semantic data while preserving canonical workspace data.
///
/// # Errors
///
/// Returns a redacted persistence error when the clear transaction fails.
pub fn clear_semantic_index(store: &mut SqliteStore) -> Result<SemanticIndexStatus, SemanticError> {
    store.clear_semantic_index().map_err(persistence_error)?;
    semantic_index_status(store)
}

/// Deterministically refreshes all registered chunks without calling a provider.
///
/// Ready vectors are reused only when `SQLite` finds the same complete profile
/// and input hash. All other current chunks become pending.
///
/// # Errors
///
/// Returns a redacted error when sources cannot be read or persisted, the
/// profile is invalid, or deterministic chunk bounds cannot be satisfied.
pub fn prepare_semantic_index(
    store: &mut SqliteStore,
    profile: &EmbeddingProfile,
    options: SemanticChunkOptions,
    updated_at: u64,
) -> Result<SemanticIndexStatus, SemanticError> {
    let sources = store.semantic_sources().map_err(persistence_error)?;
    for source in sources {
        let chunks = build_semantic_chunks(&source.node, &source.sections, profile, options)
            .map_err(|_| {
                error(
                    SemanticErrorCode::InputTooLarge,
                    "semantic input preparation exceeded configured bounds",
                )
            })?;
        store
            .replace_node_semantic_chunks(source.node.id(), profile, &chunks, updated_at)
            .map_err(persistence_error)?;
    }
    store.semantic_index_status().map_err(persistence_error)
}

/// Prepares current chunks, then computes every pending provider batch.
///
/// # Errors
///
/// Returns partial durable progress with a stable error when preparation,
/// provider computation, validation, or persistence fails.
pub async fn build_semantic_index<P: EmbeddingProvider>(
    store: &mut SqliteStore,
    provider: &P,
    profile: &EmbeddingProfile,
    options: SemanticBuildOptions,
    updated_at: u64,
) -> Result<SemanticBuildReport, SemanticBuildFailure> {
    if let Err(error) = validate_options(options) {
        return Err(failure(store, error, empty_report(store)));
    }
    if let Err(error) = prepare_semantic_index(store, profile, options.chunks, updated_at) {
        return Err(failure(store, error, empty_report(store)));
    }
    resume_semantic_index(store, provider, profile, options.batch_size, updated_at).await
}

/// Reclaims interrupted work and processes pending chunks in bounded batches.
///
/// # Errors
///
/// Returns partial durable progress when the profile is incompatible or a
/// provider, validation, or persistence operation fails.
pub async fn resume_semantic_index<P: EmbeddingProvider>(
    store: &mut SqliteStore,
    provider: &P,
    profile: &EmbeddingProfile,
    batch_size: u32,
    updated_at: u64,
) -> Result<SemanticBuildReport, SemanticBuildFailure> {
    if batch_size == 0 {
        return Err(failure(
            store,
            error(
                SemanticErrorCode::OperationFailed,
                "semantic batch size must be positive",
            ),
            empty_report(store),
        ));
    }
    let status = match store.semantic_index_status() {
        Ok(status) => status,
        Err(source) => {
            return Err(failure(
                store,
                persistence_error(source),
                empty_report(store),
            ));
        }
    };
    if status.profile.as_ref() != Some(profile) {
        return Err(failure(
            store,
            error(
                SemanticErrorCode::IncompatibleProfile,
                "active semantic profile does not match the requested profile",
            ),
            SemanticBuildReport {
                status,
                ..empty_report(store)
            },
        ));
    }
    if let Err(source) = store.recover_processing_semantic_chunks(updated_at) {
        return Err(failure(
            store,
            persistence_error(source),
            empty_report(store),
        ));
    }

    let mut report = empty_report(store);
    loop {
        let work = match store.claim_semantic_chunks(batch_size, updated_at) {
            Ok(work) => work,
            Err(source) => {
                return Err(failure(store, persistence_error(source), report));
            }
        };
        if work.is_empty() {
            report.status = current_status(store, &report.status);
            return Ok(report);
        }
        report.batches = report.batches.saturating_add(1);
        let inputs = work
            .iter()
            .map(|item| item.input.clone())
            .collect::<Vec<_>>();
        let embeddings = match provider.embed(profile, &inputs).await {
            Ok(embeddings) => embeddings,
            Err(provider_error) => {
                record_failed_batch(store, &work, provider_error.code, updated_at, &mut report);
                report.status = current_status(store, &report.status);
                return Err(SemanticBuildFailure {
                    error: provider_error,
                    report,
                });
            }
        };
        if !valid_batch(profile, work.len(), &embeddings) {
            let provider_error = error(
                SemanticErrorCode::InvalidResponse,
                "embedding provider violated the validated batch contract",
            );
            record_failed_batch(store, &work, provider_error.code, updated_at, &mut report);
            report.status = current_status(store, &report.status);
            return Err(SemanticBuildFailure {
                error: provider_error,
                report,
            });
        }
        for (item, embedding) in work.iter().zip(&embeddings) {
            match store.store_semantic_embedding(item, embedding, updated_at) {
                Ok(SemanticWriteOutcome::Stored) => {
                    report.stored = report.stored.saturating_add(1);
                }
                Ok(SemanticWriteOutcome::Stale) => {
                    report.stale = report.stale.saturating_add(1);
                }
                Err(source) => {
                    report.status = current_status(store, &report.status);
                    return Err(SemanticBuildFailure {
                        error: persistence_error(source),
                        report,
                    });
                }
            }
        }
    }
}

/// Explicitly requeues failed chunks, then resumes bounded provider work.
///
/// # Errors
///
/// Returns partial durable progress when requeueing or resumed indexing fails.
pub async fn retry_semantic_index<P: EmbeddingProvider>(
    store: &mut SqliteStore,
    provider: &P,
    profile: &EmbeddingProfile,
    batch_size: u32,
    updated_at: u64,
) -> Result<SemanticBuildReport, SemanticBuildFailure> {
    if let Err(source) = store.retry_failed_semantic_chunks(updated_at) {
        return Err(failure(
            store,
            persistence_error(source),
            empty_report(store),
        ));
    }
    resume_semantic_index(store, provider, profile, batch_size, updated_at).await
}

fn validate_options(options: SemanticBuildOptions) -> Result<(), SemanticError> {
    if options.batch_size == 0 {
        Err(error(
            SemanticErrorCode::OperationFailed,
            "semantic batch size must be positive",
        ))
    } else {
        Ok(())
    }
}

fn valid_batch(profile: &EmbeddingProfile, count: usize, embeddings: &[Vec<f32>]) -> bool {
    usize::try_from(profile.dimensions).is_ok_and(|dimensions| {
        embeddings.len() == count
            && embeddings.iter().all(|embedding| {
                embedding.len() == dimensions && embedding.iter().all(|value| value.is_finite())
            })
    })
}

fn record_failed_batch(
    store: &mut SqliteStore,
    work: &[mdtree_core::SemanticChunkWork],
    code: SemanticErrorCode,
    updated_at: u64,
    report: &mut SemanticBuildReport,
) {
    let detail = format!("embedding provider failure: {}", code_label(code));
    for item in work {
        if matches!(
            store.fail_semantic_chunk(item, &detail, updated_at),
            Ok(SemanticWriteOutcome::Stored)
        ) {
            report.failed = report.failed.saturating_add(1);
        }
    }
}

const fn code_label(code: SemanticErrorCode) -> &'static str {
    match code {
        SemanticErrorCode::NotConfigured => "not_configured",
        SemanticErrorCode::ProviderUnavailable => "provider_unavailable",
        SemanticErrorCode::Timeout => "timeout",
        SemanticErrorCode::ModelUnavailable => "model_unavailable",
        SemanticErrorCode::InputTooLarge => "input_too_large",
        SemanticErrorCode::InvalidResponse => "invalid_response",
        SemanticErrorCode::IncompatibleProfile => "incompatible_profile",
        SemanticErrorCode::PartialIndex => "partial_index",
        SemanticErrorCode::StaleWork => "stale_work",
        SemanticErrorCode::OperationFailed => "operation_failed",
    }
}

fn persistence_error(_source: mdtree_sqlite::StoreError) -> SemanticError {
    error(
        SemanticErrorCode::OperationFailed,
        "semantic persistence operation failed",
    )
}

fn empty_report(store: &SqliteStore) -> SemanticBuildReport {
    let status = store
        .semantic_index_status()
        .unwrap_or(SemanticIndexStatus {
            profile: None,
            coverage: mdtree_core::SemanticIndexCoverage {
                total: 0,
                pending: 0,
                processing: 0,
                ready: 0,
                failed: 0,
                revision: 0,
            },
        });
    SemanticBuildReport {
        batches: 0,
        stored: 0,
        stale: 0,
        failed: 0,
        status,
    }
}

fn current_status(store: &SqliteStore, fallback: &SemanticIndexStatus) -> SemanticIndexStatus {
    store
        .semantic_index_status()
        .unwrap_or_else(|_| fallback.clone())
}

fn failure(
    store: &SqliteStore,
    error: SemanticError,
    mut report: SemanticBuildReport,
) -> SemanticBuildFailure {
    report.status = current_status(store, &report.status);
    SemanticBuildFailure { error, report }
}

fn error(code: SemanticErrorCode, message: &str) -> SemanticError {
    SemanticError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use mdtree_core::{
        hash_content, hash_revision, EmbeddingMetric, EmbeddingProfile, Node, NodeFields, NodeId,
        NodeMetadata, RevisionHashInput, SemanticError, SemanticErrorCode, SemanticIndexState,
        Slug, SEMANTIC_INPUT_FORMAT_VERSION,
    };
    use mdtree_markdown::SemanticChunkOptions;
    use mdtree_sqlite::{create_workspace, SqliteStore};
    use tempfile::{tempdir, TempDir};

    use super::{
        build_semantic_index, prepare_semantic_index, resume_semantic_index, retry_semantic_index,
        EmbeddingProvider, SemanticBuildOptions,
    };

    struct Fixture {
        _directory: TempDir,
        path: PathBuf,
        store: SqliteStore,
    }

    fn workspace() -> Fixture {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("semantic-service.mdtree");
        let id = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID");
        let slug = Slug::from_str("project").expect("slug");
        let metadata = NodeMetadata::new("Project");
        let markdown_content = format!("# Project\n{}\n", "private semantic content ".repeat(30));
        let content_hash = hash_content(&markdown_content);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: id,
            parent_id: None,
            slug: &slug,
            metadata: &metadata,
            markdown_content: &markdown_content,
            sibling_order: 0,
        })
        .expect("revision hash");
        let root = Node::new(
            NodeFields {
                id,
                slug,
                metadata,
                markdown_content,
                sibling_order: 0,
                version: 1,
                content_hash,
                revision_hash,
                created_at: 1,
                updated_at: 1,
            },
            None,
        )
        .expect("root");
        let connection = create_workspace(&path, "Semantic", &root).expect("workspace");
        Fixture {
            _directory: directory,
            path,
            store: SqliteStore::new(connection),
        }
    }

    fn profile() -> EmbeddingProfile {
        EmbeddingProfile {
            provider: "ollama".into(),
            model: "embeddinggemma".into(),
            dimensions: 2,
            metric: EmbeddingMetric::Cosine,
            input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
        }
    }

    fn options() -> SemanticBuildOptions {
        SemanticBuildOptions {
            batch_size: 2,
            chunks: SemanticChunkOptions {
                max_input_bytes: 280,
                overlap_bytes: 20,
            },
        }
    }

    struct FakeProvider {
        calls: Mutex<Vec<Vec<String>>>,
        fail_call: Option<usize>,
    }

    impl FakeProvider {
        fn successful() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_call: None,
            }
        }
    }

    #[async_trait]
    impl EmbeddingProvider for FakeProvider {
        async fn embed(
            &self,
            _profile: &EmbeddingProfile,
            inputs: &[String],
        ) -> Result<Vec<Vec<f32>>, SemanticError> {
            let mut calls = self.calls.lock().expect("calls");
            let call = calls.len();
            calls.push(inputs.to_vec());
            drop(calls);
            if self.fail_call == Some(call) {
                Err(SemanticError {
                    code: SemanticErrorCode::ProviderUnavailable,
                    message: "fake provider unavailable".into(),
                })
            } else {
                Ok(inputs.iter().map(|_| vec![1.0, 0.0]).collect())
            }
        }
    }

    #[tokio::test]
    async fn build_batches_in_order_and_reaches_complete_coverage() {
        let mut fixture = workspace();
        let provider = FakeProvider::successful();

        let report = build_semantic_index(&mut fixture.store, &provider, &profile(), options(), 10)
            .await
            .expect("build");

        let calls = provider.calls.lock().expect("calls");
        assert!(calls.len() > 1);
        assert!(calls.iter().all(|batch| (1..=2).contains(&batch.len())));
        assert_eq!(report.stored, report.status.coverage.total);
        assert_eq!(report.status.coverage.state(), SemanticIndexState::Ready);
        assert_eq!(
            calls.iter().map(Vec::len).sum::<usize>(),
            usize::try_from(report.stored).expect("stored count")
        );
    }

    #[tokio::test]
    async fn partial_provider_failure_is_durable_redacted_and_retryable() {
        let mut fixture = workspace();
        let failing = FakeProvider {
            calls: Mutex::new(Vec::new()),
            fail_call: Some(1),
        };

        let failure = build_semantic_index(&mut fixture.store, &failing, &profile(), options(), 10)
            .await
            .expect_err("partial failure");

        assert_eq!(failure.error.code, SemanticErrorCode::ProviderUnavailable);
        assert!(failure.report.stored > 0);
        assert!(failure.report.failed > 0);
        assert!(failure.report.status.coverage.pending > 0);
        let errors = fixture
            .store
            .connection()
            .prepare(
                "SELECT last_error FROM semantic_chunks
                 WHERE state='failed' ORDER BY id",
            )
            .expect("failure query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("failure rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("failure details");
        assert!(errors
            .iter()
            .all(|detail| !detail.contains("private semantic content")));

        let retry = retry_semantic_index(
            &mut fixture.store,
            &FakeProvider::successful(),
            &profile(),
            2,
            11,
        )
        .await
        .expect("retry");
        assert_eq!(retry.status.coverage.state(), SemanticIndexState::Ready);
        assert!(fixture.store.root().is_ok());
    }

    #[tokio::test]
    async fn resume_reclaims_an_interrupted_processing_batch() {
        let mut fixture = workspace();
        prepare_semantic_index(&mut fixture.store, &profile(), options().chunks, 10)
            .expect("prepare");
        assert!(!fixture
            .store
            .claim_semantic_chunks(1, 11)
            .expect("claim")
            .is_empty());
        assert_eq!(
            fixture
                .store
                .semantic_index_status()
                .expect("status")
                .coverage
                .processing,
            1
        );

        let report = resume_semantic_index(
            &mut fixture.store,
            &FakeProvider::successful(),
            &profile(),
            2,
            12,
        )
        .await
        .expect("resume");
        assert_eq!(report.status.coverage.state(), SemanticIndexState::Ready);
    }

    struct StaleProvider {
        workspace: PathBuf,
    }

    #[async_trait]
    impl EmbeddingProvider for StaleProvider {
        async fn embed(
            &self,
            _profile: &EmbeddingProfile,
            inputs: &[String],
        ) -> Result<Vec<Vec<f32>>, SemanticError> {
            let store = SqliteStore::open(&self.workspace).expect("concurrent store");
            store
                .connection()
                .execute(
                    "DELETE FROM semantic_chunks
                     WHERE id=(SELECT MIN(id) FROM semantic_chunks WHERE state='processing')",
                    [],
                )
                .expect("concurrent semantic change");
            Ok(inputs.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    #[tokio::test]
    async fn stale_concurrent_results_never_recreate_changed_work() {
        let mut fixture = workspace();
        let workspace_path = fixture.path.clone();
        let report = build_semantic_index(
            &mut fixture.store,
            &StaleProvider {
                workspace: workspace_path,
            },
            &profile(),
            options(),
            10,
        )
        .await
        .expect("stale work is non-fatal");

        assert!(report.stale > 0);
        let persisted: i64 = fixture
            .store
            .connection()
            .query_row("SELECT COUNT(*) FROM semantic_chunks", [], |row| row.get(0))
            .expect("persisted count");
        assert_eq!(
            u64::try_from(persisted).expect("nonnegative count"),
            report.status.coverage.total,
            "stale responses must not recreate deleted work"
        );
    }
}
