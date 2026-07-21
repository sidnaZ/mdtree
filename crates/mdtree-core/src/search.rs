//! Stable content-search and destination-location contracts.

use serde::{Deserialize, Serialize};

use crate::{Breadcrumb, NodeId, NodeType, ReferenceType, StructuralPredicate};

/// Converts free text to an operator-safe FTS5 conjunction.
///
/// Only Unicode alphanumeric tokens are retained. Every token is quoted, so
/// punctuation and user-supplied FTS operators cannot alter query structure.
#[must_use]
pub fn normalize_fts_query(query: &str, prefix_last_token: bool) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    Some(
        tokens
            .into_iter()
            .enumerate()
            .map(|(index, token)| {
                if prefix_last_token && index == last {
                    format!("\"{token}\"*")
                } else {
                    format!("\"{token}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

/// Structural eligibility scope for content search.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// Only the selected node.
    CurrentNode,
    /// Selected node and descendants.
    Subtree,
    /// Nodes sharing the selected parent.
    Siblings,
    /// Selected parent and its descendants.
    ParentSubtree,
    /// Entire workspace.
    Workspace,
    /// Nodes connected by resolved references in either direction.
    Linked,
}

/// Optional content-search filters.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchFilters {
    /// Eligible node types; empty accepts every type.
    pub node_types: Vec<NodeType>,
    /// Required tags; empty accepts every tag set.
    pub tags: Vec<String>,
    /// Eligible outgoing status-reference types; empty accepts every status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<ReferenceType>,
    /// Inclusive minimum depth relative to the structural scope anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_depth: Option<u32>,
    /// Inclusive maximum depth relative to the structural scope anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// Inclusive minimum creation timestamp in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_from: Option<u64>,
    /// Inclusive maximum creation timestamp in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_to: Option<u64>,
    /// Inclusive minimum update timestamp in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_from: Option<u64>,
    /// Inclusive maximum update timestamp in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_to: Option<u64>,
    /// Optional canonical child-existence predicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure: Option<StructuralPredicate>,
}

impl SearchFilters {
    /// Validates range ordering and structural-scope compatibility.
    ///
    /// # Errors
    ///
    /// Returns a stable explanatory message for an invalid filter combination.
    pub fn validate(&self, scope: SearchScope) -> Result<(), &'static str> {
        if self
            .min_depth
            .zip(self.max_depth)
            .is_some_and(|(from, to)| from > to)
        {
            return Err("min_depth cannot exceed max_depth");
        }
        if self
            .created_from
            .zip(self.created_to)
            .is_some_and(|(from, to)| from > to)
        {
            return Err("created_from cannot exceed created_to");
        }
        if self
            .updated_from
            .zip(self.updated_to)
            .is_some_and(|(from, to)| from > to)
        {
            return Err("updated_from cannot exceed updated_to");
        }
        if scope == SearchScope::Linked && (self.min_depth.is_some() || self.max_depth.is_some()) {
            return Err("depth filters are not supported for linked scope");
        }
        Ok(())
    }
}

/// Complete paginated content-search request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchRequest {
    /// User query text.
    pub query: String,
    /// Structural scope.
    pub scope: SearchScope,
    /// Node anchoring non-workspace scopes.
    pub scope_node: Option<NodeId>,
    /// Metadata filters.
    #[serde(default)]
    pub filters: SearchFilters,
    /// Maximum results.
    pub limit: u32,
    /// Result offset.
    pub offset: u32,
    /// Whether the final normalized token receives an FTS prefix wildcard.
    pub prefix_last_token: bool,
}

/// One explained section-oriented content-search result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SearchMatch {
    /// Matching canonical node.
    pub node_id: NodeId,
    /// Best matching section.
    pub section_id: Option<NodeId>,
    /// Full current breadcrumb.
    pub breadcrumb: Breadcrumb,
    /// Node title.
    pub title: String,
    /// Optional node summary.
    pub summary: Option<String>,
    /// Optional node type.
    pub node_type: Option<NodeType>,
    /// Normalized 0–1 confidence.
    pub score: f64,
    /// Human-readable ranking signals.
    pub match_reasons: Vec<String>,
    /// Child count useful for structural decisions.
    pub child_count: u32,
    /// Whether the node accepts a requested child type, when supplied.
    pub accepts_child: Option<bool>,
}

/// One destination-ranking candidate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DestinationCandidate {
    /// Candidate node result and explanation.
    pub result: SearchMatch,
    /// Nearby examples following the same structural pattern.
    pub example_node_ids: Vec<NodeId>,
}

/// Suggested write action at a located destination.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocateAction {
    /// Create a narrower child below the destination.
    CreateChild,
    /// Append information to the destination itself.
    AppendToNode,
}

/// Certainty status of destination selection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocateStatus {
    /// One candidate wins by a calibrated score gap.
    Recommended,
    /// Top candidates are too close to claim certainty.
    Ambiguous,
    /// No credible candidate exists.
    NotFound,
}

/// Complete destination-location response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocateResult {
    /// Certainty status.
    pub status: LocateStatus,
    /// Suggested create-versus-append operation.
    pub action: Option<LocateAction>,
    /// Ranked candidates, best first.
    pub candidates: Vec<DestinationCandidate>,
    /// Optional proposed title for a new child.
    pub suggested_title: Option<String>,
    /// Explanation of ambiguity when applicable.
    pub ambiguity: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{normalize_fts_query, SearchFilters, SearchRequest, SearchScope};

    #[test]
    fn content_search_request_has_stable_json_contract() {
        let request = SearchRequest {
            query: "order model".into(),
            scope: SearchScope::Subtree,
            scope_node: None,
            filters: SearchFilters::default(),
            limit: 20,
            offset: 0,
            prefix_last_token: true,
        };
        assert_eq!(
            serde_json::to_value(request).expect("JSON"),
            json!({
                "query":"order model","scope":"subtree","scope_node":null,
                "filters":{"node_types":[],"tags":[]},"limit":20,"offset":0,"prefix_last_token":true
            })
        );
    }

    #[test]
    fn fts_normalization_neutralizes_operators_and_punctuation() {
        assert_eq!(
            normalize_fts_query("invoice OR * NEAR(boom)", false).as_deref(),
            Some("\"invoice\" AND \"or\" AND \"near\" AND \"boom\"")
        );
        assert_eq!(
            normalize_fts_query("order mod", true).as_deref(),
            Some("\"order\" AND \"mod\"*")
        );
        assert_eq!(normalize_fts_query(" -- !! ", true), None);
    }
}
