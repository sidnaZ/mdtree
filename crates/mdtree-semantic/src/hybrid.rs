//! Deterministic reciprocal-rank fusion.

use std::collections::BTreeMap;

use mdtree_core::{
    CursorScope, EmbeddingProfile, HybridSearchResponse, Page, PageCursor, PageLimit, PagePosition,
    PaginationError, SearchMatch, SearchMode, SearchRequest, SemanticError, SemanticErrorCode,
};
use mdtree_sqlite::{PageReadError, SqliteStore};

use crate::{search_semantic, EmbeddingProvider, SemanticSearchError};

/// Standard reciprocal-rank denominator offset.
pub const HYBRID_RRF_K: u32 = 60;
/// Candidate expansion applied before channel fusion.
pub const HYBRID_CANDIDATE_FACTOR: u32 = 4;
/// Hard maximum candidates read from either channel.
pub const MAX_HYBRID_CANDIDATES: u32 = 100;

/// Explicit behavior when the semantic channel cannot participate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HybridFallbackPolicy {
    /// Return the semantic failure and do not disguise a lexical-only result.
    #[default]
    Error,
    /// Return lexical results with explicit fallback state and explanations.
    Lexical,
}

/// Deterministic hybrid retrieval settings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HybridSearchOptions {
    /// Behavior when semantic retrieval is unavailable or incomplete.
    pub fallback: HybridFallbackPolicy,
}

/// Combines node-collapsed channel rankings with normalized reciprocal-rank fusion.
#[must_use]
pub fn fuse_rankings(lexical: &[SearchMatch], semantic: &[SearchMatch]) -> Vec<SearchMatch> {
    let mut entries = BTreeMap::<mdtree_core::NodeId, FusionEntry>::new();
    for (index, item) in lexical.iter().enumerate() {
        entries
            .entry(item.node_id)
            .or_default()
            .lexical
            .get_or_insert((
                u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1),
                item.clone(),
            ));
    }
    for (index, item) in semantic.iter().enumerate() {
        entries
            .entry(item.node_id)
            .or_default()
            .semantic
            .get_or_insert((
                u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1),
                item.clone(),
            ));
    }
    let maximum = 2.0 / f64::from(HYBRID_RRF_K + 1);
    let mut fused = entries
        .into_values()
        .map(|entry| {
            let mut result = entry.preferred().clone();
            let mut reciprocal = 0.0;
            let mut reasons = Vec::new();
            if let Some((rank, lexical)) = entry.lexical {
                reciprocal += reciprocal_rank(rank);
                reasons.push(format!("lexical rank {rank}"));
                reasons.extend(
                    lexical
                        .match_reasons
                        .into_iter()
                        .map(|reason| format!("lexical: {reason}")),
                );
            }
            if let Some((rank, semantic)) = entry.semantic {
                reciprocal += reciprocal_rank(rank);
                reasons.push(format!("semantic rank {rank}"));
                reasons.extend(
                    semantic
                        .match_reasons
                        .into_iter()
                        .map(|reason| format!("semantic: {reason}")),
                );
            }
            result.score = (reciprocal / maximum).clamp(0.0, 1.0);
            result.match_reasons = reasons;
            result
        })
        .collect::<Vec<_>>();
    fused.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    fused
}

/// Retrieves bounded lexical and semantic candidates, then fuses one page.
///
/// # Errors
///
/// Returns semantic/provider failures unless explicit lexical fallback is
/// selected, and preserves storage and pagination failures in all modes.
pub async fn search_hybrid<P: EmbeddingProvider>(
    store: &SqliteStore,
    provider: &P,
    profile: &EmbeddingProfile,
    request: &SearchRequest,
    limit: PageLimit,
    cursor: Option<&PageCursor>,
    options: HybridSearchOptions,
) -> Result<HybridSearchResponse, SemanticSearchError> {
    if request.mode != SearchMode::Hybrid {
        return Err(semantic_error(
            SemanticErrorCode::OperationFailed,
            "hybrid search requires hybrid mode",
        )
        .into());
    }
    if request.query.trim().is_empty() {
        return Err(semantic_error(
            SemanticErrorCode::OperationFailed,
            "hybrid search query must not be blank",
        )
        .into());
    }
    let workspace_revision = store.workspace_revision().map_err(page_store)?;
    let index = store.semantic_index_status().map_err(page_store)?;
    let index_revision = index.coverage.revision;
    let request_key = serde_json::to_string(&(
        &request.query,
        request.mode,
        request.scope,
        &request.scope_node,
        &request.filters,
        profile,
        options.fallback as u8,
        HYBRID_RRF_K,
        HYBRID_CANDIDATE_FACTOR,
    ))
    .map_err(|_| {
        semantic_error(
            SemanticErrorCode::OperationFailed,
            "hybrid request could not be normalized",
        )
    })?;
    let scope = CursorScope::new("hybrid_search", request.scope_node, &request_key)
        .map_err(PageReadError::from)?;
    let offset = match cursor
        .map(|value| value.resume_indexed(&scope, workspace_revision, index_revision))
        .transpose()
        .map_err(PageReadError::from)?
    {
        None => 0,
        Some(PagePosition::Search { offset }) => offset,
        Some(_) => {
            return Err(SemanticSearchError::Page(PageReadError::from(
                PaginationError::InvalidCursorPosition,
            )));
        }
    };
    let candidate_limit = hybrid_candidate_limit(offset, limit.get());

    let mut lexical_request = request.clone();
    lexical_request.mode = SearchMode::Lexical;
    lexical_request.offset = 0;
    lexical_request.limit = candidate_limit;
    let lexical = store.search_content(&lexical_request).map_err(page_store)?;

    let mut semantic_request = request.clone();
    semantic_request.mode = SearchMode::Semantic;
    semantic_request.offset = 0;
    semantic_request.limit = candidate_limit;
    let semantic_result = search_semantic(
        store,
        provider,
        profile,
        &semantic_request,
        PageLimit::new(candidate_limit).map_err(PageReadError::from)?,
        None,
    )
    .await;
    let (semantic, scanned_chunks, fallback) = match semantic_result {
        Ok(response) => (response.matches.items, response.scanned_chunks, None),
        Err(SemanticSearchError::Semantic(error))
            if options.fallback == HybridFallbackPolicy::Lexical =>
        {
            (Vec::new(), 0, Some(error.code))
        }
        Err(error) => return Err(error),
    };

    let mut fused = fuse_rankings(&lexical, &semantic);
    if let Some(code) = fallback {
        let reason = format!("hybrid lexical fallback: {}", code_label(code));
        for item in &mut fused {
            item.match_reasons.push(reason.clone());
        }
    }
    finish_hybrid_page(
        store,
        fused,
        index,
        scanned_chunks,
        fallback,
        HybridPageContext {
            workspace_revision,
            index_revision,
            scope,
            offset,
            limit,
        },
    )
}

struct HybridPageContext {
    workspace_revision: u64,
    index_revision: u64,
    scope: CursorScope,
    offset: u32,
    limit: PageLimit,
}

fn finish_hybrid_page(
    store: &SqliteStore,
    fused: Vec<SearchMatch>,
    index: mdtree_core::SemanticIndexStatus,
    scanned_chunks: u64,
    fallback: Option<SemanticErrorCode>,
    context: HybridPageContext,
) -> Result<HybridSearchResponse, SemanticSearchError> {
    let HybridPageContext {
        workspace_revision,
        index_revision,
        scope,
        offset,
        limit,
    } = context;
    let offset_usize = usize::try_from(offset).unwrap_or(usize::MAX);
    let take = usize::try_from(limit.get()).unwrap_or(usize::MAX);
    let mut items = fused
        .into_iter()
        .skip(offset_usize)
        .take(take.saturating_add(1))
        .collect::<Vec<_>>();
    let has_more = items.len() > take;
    if has_more {
        items.pop();
    }
    ensure_revisions(store, workspace_revision, index_revision)?;
    let next_cursor = if has_more {
        let returned = u32::try_from(items.len()).unwrap_or(u32::MAX);
        Some(
            PageCursor::issue_indexed(
                workspace_revision,
                index_revision,
                scope,
                PagePosition::Search {
                    offset: offset.saturating_add(returned),
                },
            )
            .map_err(PageReadError::from)?,
        )
    } else {
        None
    };
    Ok(HybridSearchResponse {
        matches: Page::new(items, next_cursor),
        index,
        scanned_chunks,
        fallback,
    })
}

#[derive(Default)]
struct FusionEntry {
    lexical: Option<(u32, SearchMatch)>,
    semantic: Option<(u32, SearchMatch)>,
}

impl FusionEntry {
    fn preferred(&self) -> &SearchMatch {
        match (&self.lexical, &self.semantic) {
            (Some((lexical_rank, lexical)), Some((semantic_rank, semantic))) => {
                if lexical_rank <= semantic_rank {
                    lexical
                } else {
                    semantic
                }
            }
            (Some((_, lexical)), None) => lexical,
            (None, Some((_, semantic))) => semantic,
            (None, None) => unreachable!("fusion entries always contain a ranking"),
        }
    }
}

fn reciprocal_rank(rank: u32) -> f64 {
    1.0 / f64::from(HYBRID_RRF_K.saturating_add(rank))
}

fn hybrid_candidate_limit(offset: u32, page_limit: u32) -> u32 {
    offset
        .saturating_add(page_limit)
        .saturating_mul(HYBRID_CANDIDATE_FACTOR)
        .clamp(page_limit, MAX_HYBRID_CANDIDATES)
}

fn ensure_revisions(
    store: &SqliteStore,
    workspace_revision: u64,
    index_revision: u64,
) -> Result<(), SemanticSearchError> {
    let current_workspace = store.workspace_revision().map_err(page_store)?;
    if current_workspace != workspace_revision {
        return Err(PageReadError::from(PaginationError::StaleCursor {
            cursor_revision: workspace_revision,
            current_revision: current_workspace,
        })
        .into());
    }
    let current_index = store
        .semantic_index_status()
        .map_err(page_store)?
        .coverage
        .revision;
    if current_index != index_revision {
        return Err(PageReadError::from(PaginationError::StaleIndexCursor {
            cursor_revision: index_revision,
            current_revision: current_index,
        })
        .into());
    }
    Ok(())
}

fn page_store(error: mdtree_sqlite::StoreError) -> SemanticSearchError {
    SemanticSearchError::Page(PageReadError::Store(error))
}

fn semantic_error(code: SemanticErrorCode, message: &str) -> SemanticError {
    SemanticError {
        code,
        message: message.into(),
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{Breadcrumb, NodeId, SearchMatch};
    use serde::Deserialize;

    use super::{fuse_rankings, hybrid_candidate_limit};

    #[derive(Deserialize)]
    struct RelevanceCase {
        name: String,
        lexical: Vec<String>,
        semantic: Vec<String>,
        expected: Vec<String>,
    }

    fn item(raw: &str, section_suffix: &str, reason: &str) -> SearchMatch {
        let node_id = NodeId::from_str(raw).expect("node ID");
        let section_id = NodeId::from_str(section_suffix).expect("section ID");
        SearchMatch {
            node_id,
            section_id: Some(section_id),
            breadcrumb: Breadcrumb::new(vec![raw.into()]).expect("breadcrumb"),
            title: raw.into(),
            summary: None,
            node_type: None,
            score: 0.99,
            match_reasons: vec![reason.into()],
            child_count: 0,
            accepts_child: None,
        }
    }

    fn list(ids: &[String], channel: &str) -> Vec<SearchMatch> {
        ids.iter()
            .map(|id| item(id, id, &format!("{channel} fixture signal")))
            .collect()
    }

    #[test]
    fn checked_in_relevance_cases_have_stable_expected_ranks() {
        let cases: Vec<RelevanceCase> =
            serde_json::from_str(include_str!("../tests/fixtures/hybrid_relevance.json"))
                .expect("relevance fixture");
        for case in cases {
            let fused = fuse_rankings(
                &list(&case.lexical, "lexical"),
                &list(&case.semantic, "semantic"),
            );
            assert_eq!(
                fused
                    .iter()
                    .map(|item| item.node_id.to_string())
                    .collect::<Vec<_>>(),
                case.expected,
                "{}",
                case.name
            );
            assert!(fused.iter().all(|item| (0.0..=1.0).contains(&item.score)));
        }
    }

    #[test]
    fn overlap_merges_explanations_and_lexical_wins_equal_rank_section_choice() {
        let node = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
        let lexical_section = "01JZ8Q5CWPN8T7KPN5A1V9B6XN";
        let semantic_section = "01JZ8Q5CWPN8T7KPN5A1V9B6XP";
        let lexical = vec![
            item(node, lexical_section, "title matched"),
            item(node, semantic_section, "duplicate must collapse"),
        ];
        let semantic = vec![item(node, semantic_section, "cosine similarity 0.900000")];

        let fused = fuse_rankings(&lexical, &semantic);

        assert_eq!(fused.len(), 1);
        assert_eq!(
            fused[0].section_id,
            Some(NodeId::from_str(lexical_section).expect("section"))
        );
        assert!((fused[0].score - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            fused[0].match_reasons,
            vec![
                "lexical rank 1",
                "lexical: title matched",
                "semantic rank 1",
                "semantic: cosine similarity 0.900000"
            ]
        );
    }

    #[test]
    fn candidate_expansion_is_bounded_and_saturating() {
        assert_eq!(hybrid_candidate_limit(0, 10), 40);
        assert_eq!(hybrid_candidate_limit(20, 10), 100);
        assert_eq!(hybrid_candidate_limit(u32::MAX, 100), 100);
    }
}
