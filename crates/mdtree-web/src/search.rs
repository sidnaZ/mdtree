//! Cross-workspace node search backing the header search box.
//!
//! Default matching is a plain, case-insensitive title/slug substring match.
//! Three qualifiers layer on top of (or replace) that default: `workspace:`
//! restricts which open workspace(s) are searched, `slug:` matches only the
//! slug, and `text:` runs the existing weighted section-content search
//! (`SqliteStore::search_content`) instead of a plain substring match.

use std::collections::HashSet;

use axum::extract::{Query, State};
use axum::Json;
use mdtree_core::{NodeId, SearchFilters, SearchRequest, SearchScope, Slug};
use serde::{Deserialize, Serialize};

use crate::api::ApiError;
use crate::state::AppState;

/// Total matches returned across every searched workspace.
const MAX_RESULTS: usize = 50;
/// Per-workspace cap applied before the cross-workspace merge, so one large
/// workspace can't crowd out matches from every other open workspace.
const MAX_RESULTS_PER_WORKSPACE: usize = 50;

#[derive(Deserialize)]
pub(crate) struct SearchParams {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
pub(crate) struct SearchResultItem {
    workspace_id: usize,
    workspace_name: String,
    node_id: String,
    slug: String,
    title: String,
    /// Canonical root-to-node slug path, for display.
    path: String,
    /// Root-to-parent ancestor IDs, so the client can expand (and load, if
    /// not yet cached) each one to bring the match into view in the tree.
    ancestor_ids: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct SearchResponse {
    matches: Vec<SearchResultItem>,
}

#[derive(Default)]
struct ParsedQuery {
    workspace: Option<String>,
    slug: Option<String>,
    text: Option<String>,
    /// Free text outside any qualifier; matched against title and slug.
    default: Option<String>,
}

const QUALIFIERS: [&str; 3] = ["workspace:", "slug:", "text:"];

/// Lowercases only ASCII bytes, leaving every other byte untouched. Plain
/// `str::to_lowercase` can change a string's byte length for some non-ASCII
/// characters, which would invalidate byte offsets found in the lowercased
/// copy when applied back to the original for slicing; since the qualifier
/// keywords themselves are pure ASCII, this is enough to find them safely
/// while leaving offsets valid for arbitrary Unicode workspace/node names.
fn ascii_lower(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Parses `raw` into its qualifiers and default free text. A qualifier only
/// matches at a word boundary (string start or preceding whitespace) so it
/// can't trigger in the middle of an unrelated word.
fn parse_query(raw: &str) -> ParsedQuery {
    let lower = ascii_lower(raw);
    let mut hits: Vec<(usize, &str)> = Vec::new();
    let mut cursor = 0;
    while cursor <= lower.len() {
        let next = QUALIFIERS
            .iter()
            .filter_map(|qualifier| {
                lower[cursor..]
                    .find(qualifier)
                    .map(|offset| (cursor + offset, *qualifier))
            })
            .filter(|(pos, _)| *pos == 0 || lower.as_bytes()[*pos - 1].is_ascii_whitespace())
            .min_by_key(|(pos, _)| *pos);
        match next {
            Some((pos, qualifier)) => {
                hits.push((pos, qualifier));
                cursor = pos + qualifier.len();
            }
            None => break,
        }
    }

    let mut parsed = ParsedQuery::default();
    let default_end = hits.first().map_or(raw.len(), |(pos, _)| *pos);
    let default_text = raw[..default_end].trim();
    if !default_text.is_empty() {
        parsed.default = Some(default_text.to_string());
    }
    for (index, (pos, qualifier)) in hits.iter().enumerate() {
        let value_start = pos + qualifier.len();
        let value_end = hits
            .get(index + 1)
            .map_or(raw.len(), |(next_pos, _)| *next_pos);
        let value = unquote(raw[value_start..value_end].trim());
        if value.is_empty() {
            continue;
        }
        match *qualifier {
            "workspace:" => parsed.workspace = Some(value),
            "slug:" => parsed.slug = Some(value),
            "text:" => parsed.text = Some(value),
            _ => unreachable!("QUALIFIERS is exhaustively matched here"),
        }
    }
    parsed
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

pub(crate) async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let parsed = parse_query(&params.q);
    if parsed.workspace.is_none()
        && parsed.slug.is_none()
        && parsed.text.is_none()
        && parsed.default.is_none()
    {
        return Ok(Json(SearchResponse {
            matches: Vec::new(),
        }));
    }

    let mut matches = Vec::new();
    for (workspace_id, workspace) in state.workspaces.iter().enumerate() {
        if let Some(filter) = &parsed.workspace {
            if !contains_ignore_case(&workspace.name, filter) {
                continue;
            }
        }
        let store = workspace
            .store
            .lock()
            .expect("workspace store mutex poisoned");

        let text_hits: Option<HashSet<NodeId>> = match &parsed.text {
            Some(text) => Some(
                store
                    .search_content(&SearchRequest {
                        query: text.clone(),
                        scope: SearchScope::Workspace,
                        scope_node: None,
                        filters: SearchFilters::default(),
                        limit: 1000,
                        offset: 0,
                        prefix_last_token: true,
                    })?
                    .into_iter()
                    .map(|item| item.node_id)
                    .collect(),
            ),
            None => None,
        };

        let mut workspace_matches = Vec::new();
        for item in store.subtree(workspace.root)? {
            let node = item.node;
            let title = node.fields().metadata.title.clone();
            let slug = node.fields().slug.as_str().to_string();
            if let Some(filter) = &parsed.slug {
                if !contains_ignore_case(&slug, filter) {
                    continue;
                }
            }
            if let Some(term) = &parsed.default {
                if !contains_ignore_case(&title, term) && !contains_ignore_case(&slug, term) {
                    continue;
                }
            }
            if let Some(hits) = &text_hits {
                if !hits.contains(&node.id()) {
                    continue;
                }
            }
            let ancestor_ids = store
                .ancestors(node.id())?
                .into_iter()
                .map(|depth| depth.node.id().to_string())
                .collect::<Vec<_>>();
            let path = store
                .canonical_path(node.id())?
                .iter()
                .map(Slug::as_str)
                .collect::<Vec<_>>()
                .join("/");
            workspace_matches.push(SearchResultItem {
                workspace_id,
                workspace_name: workspace.name.clone(),
                node_id: node.id().to_string(),
                slug,
                title,
                path,
                ancestor_ids,
            });
            if workspace_matches.len() >= MAX_RESULTS_PER_WORKSPACE {
                break;
            }
        }
        matches.extend(workspace_matches);
    }
    matches.truncate(MAX_RESULTS);
    Ok(Json(SearchResponse { matches }))
}

#[cfg(test)]
mod tests {
    use super::parse_query;

    #[test]
    fn plain_text_is_the_default_term() {
        let parsed = parse_query("orders table");
        assert_eq!(parsed.default.as_deref(), Some("orders table"));
        assert!(parsed.workspace.is_none());
        assert!(parsed.slug.is_none());
        assert!(parsed.text.is_none());
    }

    #[test]
    fn qualifiers_split_into_their_own_fields() {
        let parsed = parse_query("workspace: MDTree slug: search text: header");
        assert_eq!(parsed.workspace.as_deref(), Some("MDTree"));
        assert_eq!(parsed.slug.as_deref(), Some("search"));
        assert_eq!(parsed.text.as_deref(), Some("header"));
        assert!(parsed.default.is_none());
    }

    #[test]
    fn a_qualifier_keyword_inside_a_word_is_not_recognized() {
        let parsed = parse_query("subworkspace:notes");
        assert_eq!(parsed.default.as_deref(), Some("subworkspace:notes"));
        assert!(parsed.workspace.is_none());
    }

    #[test]
    fn quoted_qualifier_values_may_contain_spaces() {
        let parsed = parse_query(r#"workspace: "My Workspace" text: foo bar"#);
        assert_eq!(parsed.workspace.as_deref(), Some("My Workspace"));
        assert_eq!(parsed.text.as_deref(), Some("foo bar"));
    }

    #[test]
    fn non_ascii_text_around_a_qualifier_keeps_offsets_valid() {
        let parsed = parse_query("Rīga slug: rīga-pilsēta");
        assert_eq!(parsed.default.as_deref(), Some("Rīga"));
        assert_eq!(parsed.slug.as_deref(), Some("rīga-pilsēta"));
    }
}
