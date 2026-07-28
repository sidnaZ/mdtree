//! Query embedding and exact semantic retrieval orchestration.

use mdtree_core::{
    EmbeddingProfile, Page, PageCursor, PageLimit, SearchMode, SearchRequest, SemanticError,
    SemanticErrorCode, SemanticIndexState, SemanticSearchResponse, SEMANTIC_INPUT_FORMAT_VERSION,
};
use mdtree_sqlite::{PageReadError, SqliteStore};
use thiserror::Error;

use crate::EmbeddingProvider;

/// Provider, compatibility, persistence, or cursor failure from semantic search.
#[derive(Debug, Error)]
pub enum SemanticSearchError {
    /// Stable provider-independent semantic failure.
    #[error(transparent)]
    Semantic(#[from] SemanticError),
    /// Storage or pagination failure after query embedding.
    #[error(transparent)]
    Page(#[from] PageReadError),
}

/// Embeds one query and runs exact eligible-vector retrieval.
///
/// # Errors
///
/// Returns stable configuration, coverage, provider, storage, and cursor
/// failures. Partial indexes are rejected instead of silently searched.
pub async fn search_semantic<P: EmbeddingProvider>(
    store: &SqliteStore,
    provider: &P,
    profile: &EmbeddingProfile,
    request: &SearchRequest,
    limit: PageLimit,
    cursor: Option<&PageCursor>,
) -> Result<SemanticSearchResponse, SemanticSearchError> {
    if request.mode != SearchMode::Semantic {
        return Err(error(
            SemanticErrorCode::OperationFailed,
            "semantic search requires semantic mode",
        )
        .into());
    }
    if request.query.trim().is_empty() {
        return Err(error(
            SemanticErrorCode::OperationFailed,
            "semantic search query must not be blank",
        )
        .into());
    }
    let index = store.semantic_index_status().map_err(|_| {
        error(
            SemanticErrorCode::OperationFailed,
            "semantic index status could not be read",
        )
    })?;
    let Some(active_profile) = index.profile.as_ref() else {
        return Err(error(
            SemanticErrorCode::NotConfigured,
            "semantic index is not configured",
        )
        .into());
    };
    if active_profile != profile {
        return Err(error(
            SemanticErrorCode::IncompatibleProfile,
            "requested semantic profile is not active",
        )
        .into());
    }
    match index.coverage.state() {
        SemanticIndexState::Empty => {
            if cursor.is_some() {
                return Err(error(
                    SemanticErrorCode::StaleWork,
                    "empty semantic index cannot resume a continuation",
                )
                .into());
            }
            return Ok(SemanticSearchResponse {
                matches: Page::new(Vec::new(), None),
                index,
                scanned_chunks: 0,
            });
        }
        SemanticIndexState::Ready => {}
        SemanticIndexState::Pending | SemanticIndexState::Partial | SemanticIndexState::Failed => {
            return Err(error(
                SemanticErrorCode::PartialIndex,
                "semantic index does not completely cover current chunks",
            )
            .into());
        }
    }

    let input = format!(
        "mdtree_semantic_query_v{SEMANTIC_INPUT_FORMAT_VERSION}\nquery=\n{}",
        request.query
    );
    let mut embeddings = provider.embed(profile, &[input]).await?;
    let query_embedding = embeddings.pop().ok_or_else(|| {
        error(
            SemanticErrorCode::InvalidResponse,
            "embedding provider returned no query vector",
        )
    })?;
    store
        .search_semantic_page(request, profile, &query_embedding, limit, cursor)
        .map_err(Into::into)
}

fn error(code: SemanticErrorCode, message: &str) -> SemanticError {
    SemanticError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use mdtree_core::{
        hash_content, hash_revision, EmbeddingMetric, EmbeddingProfile, Node, NodeFields, NodeId,
        NodeMetadata, RevisionHashInput, SearchFilters, SearchMode, SearchRequest, SearchScope,
        SemanticError, SemanticErrorCode, Slug, SEMANTIC_INPUT_FORMAT_VERSION,
    };
    use mdtree_markdown::SemanticChunkOptions;
    use mdtree_sqlite::{create_workspace, SqliteStore};
    use tempfile::{tempdir, TempDir};

    use super::{search_semantic, EmbeddingProvider, PageLimit};
    use crate::{
        build_semantic_index, search_hybrid, HybridFallbackPolicy, HybridSearchOptions,
        SemanticBuildOptions,
    };

    struct Fixture {
        _directory: TempDir,
        store: SqliteStore,
    }

    fn workspace() -> Fixture {
        workspace_with_content("# Orders\nCanonical purchase records.\n")
    }

    fn workspace_with_content(content: &str) -> Fixture {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("semantic-search.mdtree");
        let id = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID");
        let slug = Slug::from_str("purchases").expect("slug");
        let metadata = NodeMetadata::new("Orders");
        let markdown_content = content.to_owned();
        let root = Node::new(
            NodeFields {
                id,
                slug: slug.clone(),
                metadata: metadata.clone(),
                content_hash: hash_content(&markdown_content),
                revision_hash: hash_revision(RevisionHashInput {
                    node_id: id,
                    parent_id: None,
                    slug: &slug,
                    metadata: &metadata,
                    markdown_content: &markdown_content,
                    sibling_order: 0,
                })
                .expect("revision"),
                markdown_content,
                sibling_order: 0,
                version: 1,
                created_at: 1,
                updated_at: 1,
            },
            None,
        )
        .expect("root");
        let connection = create_workspace(&path, "Search", &root).expect("workspace");
        Fixture {
            _directory: directory,
            store: SqliteStore::new(connection),
        }
    }

    fn profile(model: &str) -> EmbeddingProfile {
        EmbeddingProfile {
            provider: "ollama".into(),
            model: model.into(),
            dimensions: 2,
            metric: EmbeddingMetric::Cosine,
            input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
        }
    }

    fn request() -> SearchRequest {
        SearchRequest {
            query: "where purchases are recorded".into(),
            mode: SearchMode::Semantic,
            scope: SearchScope::Workspace,
            scope_node: None,
            filters: SearchFilters::default(),
            limit: 20,
            offset: 0,
            prefix_last_token: false,
        }
    }

    struct StaticProvider {
        calls: AtomicUsize,
        failure: Option<SemanticErrorCode>,
    }

    impl StaticProvider {
        fn successful() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                failure: None,
            }
        }
    }

    #[async_trait]
    impl EmbeddingProvider for StaticProvider {
        async fn embed(
            &self,
            _profile: &EmbeddingProfile,
            inputs: &[String],
        ) -> Result<Vec<Vec<f32>>, SemanticError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = self.failure {
                Err(SemanticError {
                    code,
                    message: "controlled provider failure".into(),
                })
            } else {
                Ok(inputs.iter().map(|_| vec![1.0, 0.0]).collect())
            }
        }
    }

    #[tokio::test]
    async fn embeds_a_conceptual_query_and_returns_profile_coverage() {
        let mut fixture = workspace();
        let profile = profile("fixture");
        let provider = StaticProvider::successful();
        build_semantic_index(
            &mut fixture.store,
            &provider,
            &profile,
            SemanticBuildOptions {
                batch_size: 4,
                chunks: SemanticChunkOptions::default(),
            },
            10,
        )
        .await
        .expect("index");

        let response = search_semantic(
            &fixture.store,
            &provider,
            &profile,
            &request(),
            PageLimit::new(10).expect("limit"),
            None,
        )
        .await
        .expect("semantic search");

        assert_eq!(response.matches.items[0].title, "Orders");
        assert_eq!(response.index.profile, Some(profile));
        assert_eq!(response.scanned_chunks, 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn conceptual_synonym_queries_outperform_lexical_token_matching() {
        let mut fixture = workspace_with_content(
            "# Orders\nRetries use exponential backoff after a declined charge.\n",
        );
        let profile = profile("synonym-fixture");
        let provider = StaticProvider::successful();
        build_semantic_index(
            &mut fixture.store,
            &provider,
            &profile,
            SemanticBuildOptions::default(),
            10,
        )
        .await
        .expect("index");

        for query in [
            "how are failed payments tried again",
            "card refusal handling",
            "delay strategy for payment errors",
        ] {
            let mut semantic_request = request();
            semantic_request.query = query.into();
            let mut lexical_request = semantic_request.clone();
            lexical_request.mode = SearchMode::Lexical;
            assert!(
                fixture
                    .store
                    .search_content(&lexical_request)
                    .expect("lexical")
                    .is_empty(),
                "{query}"
            );
            let semantic = search_semantic(
                &fixture.store,
                &provider,
                &profile,
                &semantic_request,
                PageLimit::new(10).expect("limit"),
                None,
            )
            .await
            .expect("semantic");
            assert_eq!(semantic.matches.items[0].title, "Orders", "{query}");
        }
    }

    #[tokio::test]
    async fn empty_partial_incompatible_and_provider_failures_are_explicit() {
        let mut fixture = workspace();
        let active = profile("active");
        fixture
            .store
            .activate_semantic_profile(&active)
            .expect("activate");
        let provider = StaticProvider::successful();
        let empty = search_semantic(
            &fixture.store,
            &provider,
            &active,
            &request(),
            PageLimit::new(10).expect("limit"),
            None,
        )
        .await
        .expect("explicit empty index");
        assert!(empty.matches.items.is_empty());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

        crate::prepare_semantic_index(
            &mut fixture.store,
            &active,
            SemanticChunkOptions::default(),
            10,
        )
        .expect("pending index");
        let partial = search_semantic(
            &fixture.store,
            &provider,
            &active,
            &request(),
            PageLimit::new(10).expect("limit"),
            None,
        )
        .await
        .expect_err("partial index");
        assert!(matches!(
            partial,
            super::SemanticSearchError::Semantic(SemanticError {
                code: SemanticErrorCode::PartialIndex,
                ..
            })
        ));

        let incompatible = search_semantic(
            &fixture.store,
            &provider,
            &profile("other"),
            &request(),
            PageLimit::new(10).expect("limit"),
            None,
        )
        .await
        .expect_err("incompatible profile");
        assert!(matches!(
            incompatible,
            super::SemanticSearchError::Semantic(SemanticError {
                code: SemanticErrorCode::IncompatibleProfile,
                ..
            })
        ));

        build_semantic_index(
            &mut fixture.store,
            &provider,
            &active,
            SemanticBuildOptions::default(),
            11,
        )
        .await
        .expect("complete index");
        let failing = StaticProvider {
            calls: AtomicUsize::new(0),
            failure: Some(SemanticErrorCode::Timeout),
        };
        let provider_error = search_semantic(
            &fixture.store,
            &failing,
            &active,
            &request(),
            PageLimit::new(10).expect("limit"),
            None,
        )
        .await
        .expect_err("query provider error");
        assert!(matches!(
            provider_error,
            super::SemanticSearchError::Semantic(SemanticError {
                code: SemanticErrorCode::Timeout,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn hybrid_fallback_and_two_channel_explanations_are_explicit() {
        let mut fixture = workspace();
        let profile = profile("hybrid");
        crate::prepare_semantic_index(
            &mut fixture.store,
            &profile,
            SemanticChunkOptions::default(),
            10,
        )
        .expect("pending index");
        let provider = StaticProvider::successful();
        let mut hybrid_request = request();
        hybrid_request.query = "purchase".into();
        hybrid_request.mode = SearchMode::Hybrid;

        let fallback = search_hybrid(
            &fixture.store,
            &provider,
            &profile,
            &hybrid_request,
            PageLimit::new(10).expect("limit"),
            None,
            HybridSearchOptions {
                fallback: HybridFallbackPolicy::Lexical,
            },
        )
        .await
        .expect("lexical fallback");
        assert_eq!(fallback.fallback, Some(SemanticErrorCode::PartialIndex));
        assert_eq!(fallback.matches.items[0].title, "Orders");
        assert!(fallback.matches.items[0]
            .match_reasons
            .iter()
            .any(|reason| reason == "hybrid lexical fallback: partial_index"));

        let strict = search_hybrid(
            &fixture.store,
            &provider,
            &profile,
            &hybrid_request,
            PageLimit::new(10).expect("limit"),
            None,
            HybridSearchOptions::default(),
        )
        .await
        .expect_err("strict hybrid");
        assert!(matches!(
            strict,
            super::SemanticSearchError::Semantic(SemanticError {
                code: SemanticErrorCode::PartialIndex,
                ..
            })
        ));

        build_semantic_index(
            &mut fixture.store,
            &provider,
            &profile,
            SemanticBuildOptions::default(),
            11,
        )
        .await
        .expect("complete index");
        let hybrid = search_hybrid(
            &fixture.store,
            &provider,
            &profile,
            &hybrid_request,
            PageLimit::new(10).expect("limit"),
            None,
            HybridSearchOptions::default(),
        )
        .await
        .expect("hybrid search");
        assert!(hybrid.fallback.is_none());
        assert!(hybrid.matches.items[0]
            .match_reasons
            .iter()
            .any(|reason| reason.starts_with("lexical rank")));
        assert!(hybrid.matches.items[0]
            .match_reasons
            .iter()
            .any(|reason| reason.starts_with("semantic rank")));
    }
}
