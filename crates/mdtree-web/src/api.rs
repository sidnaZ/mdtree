//! Bounded read endpoints: session metadata, node children, and rendered
//! Markdown. Every read goes through the existing `SqliteStore` — no tree,
//! version, or path logic is duplicated here.

use std::str::FromStr;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use mdtree_core::{Node, NodeId, NodeSelector, ReferenceTarget};
use mdtree_sqlite::{SqliteStore, StoreError};
use serde::Serialize;

use crate::markdown::render_sanitized_html;
use crate::state::AppState;

/// A small error for read-endpoint handlers.
pub(crate) enum ApiError {
    NotFound,
    Internal(StoreError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "node not found").into_response(),
            ApiError::Internal(error) => {
                (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self::Internal(error)
    }
}

#[derive(Serialize)]
pub(crate) struct WorkspaceSummary {
    id: usize,
    name: String,
    root: String,
    /// Every relation type this workspace uses, regardless of whether any
    /// node carrying one has actually been loaded yet — lets the client
    /// show a complete relations legend up front instead of only relation
    /// types discovered as specific nodes happen to be fetched.
    relation_types: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct WorkspacesResponse {
    session_credential: String,
    /// The running `mdtree-web` crate version, shown next to the "`MDTree`"
    /// header caption. Shared with every other crate via `version.workspace`,
    /// so this is the same version as the `mdtree` CLI binary.
    server_version: &'static str,
    workspaces: Vec<WorkspaceSummary>,
}

pub(crate) async fn workspaces(
    State(state): State<AppState>,
) -> Result<Json<WorkspacesResponse>, ApiError> {
    let mut workspaces = Vec::with_capacity(state.workspaces.len());
    for (id, workspace) in state.workspaces.iter().enumerate() {
        let store = workspace
            .store
            .lock()
            .expect("workspace store mutex poisoned");
        workspaces.push(WorkspaceSummary {
            id,
            name: workspace.name.clone(),
            root: workspace.root.to_string(),
            relation_types: store.all_relation_types()?,
        });
    }
    Ok(Json(WorkspacesResponse {
        session_credential: state.session_credential.to_string(),
        server_version: env!("CARGO_PKG_VERSION"),
        workspaces,
    }))
}

/// Looks up the workspace addressed by a `{workspace}` path segment.
fn resolve_workspace(
    state: &AppState,
    workspace: usize,
) -> Result<&crate::state::WorkspaceState, ApiError> {
    state.workspaces.get(workspace).ok_or(ApiError::NotFound)
}

#[derive(Serialize)]
pub(crate) struct NodeSummary {
    id: String,
    slug: String,
    /// Canonical root-to-node slug path produced by the persistence service.
    path: String,
    title: String,
    /// Direct child count, shown in the tree canvas without requiring the
    /// child itself to be expanded/fetched first.
    children_count: u64,
    /// Every outgoing typed reference from this node (e.g. `done`,
    /// `in-progress`), shown as small colored, clickable indicators on the
    /// node and in a canvas legend. Reference types are free-text, not a
    /// fixed enum, so this is whatever the workspace actually uses — never a
    /// hardcoded set. Unlike a plain type list, each entry keeps its own
    /// target so a specific reference (not just its type) can be hovered for
    /// a tooltip and clicked to navigate to what it actually points at.
    references: Vec<ReferenceSummary>,
    /// Needed by structural-editing commands (reorder, reparent) for
    /// optimistic-concurrency preconditions on a node the client has only
    /// ever seen as a summary, not fetched in full.
    version: u64,
}

#[derive(Serialize)]
pub(crate) struct NodeResponse {
    #[serde(flatten)]
    summary: NodeSummary,
    children: Vec<NodeSummary>,
}

#[derive(Serialize)]
pub(crate) struct ReferenceSummary {
    reference_type: String,
    #[serde(flatten)]
    target: ReferenceTargetSummary,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ReferenceTargetSummary {
    Resolved {
        node_id: String,
        title: String,
        path: String,
    },
    Unresolved {
        target_ref: String,
    },
}

fn summarize(
    node: &Node,
    path: String,
    children_count: u64,
    references: Vec<ReferenceSummary>,
) -> NodeSummary {
    NodeSummary {
        id: node.id().to_string(),
        slug: node.fields().slug.as_str().to_string(),
        path,
        title: node.fields().metadata.title.clone(),
        children_count,
        references,
        version: node.fields().version,
    }
}

/// Every outgoing reference from one node, each carrying enough about its
/// target to render a tooltip without a follow-up request. The actual
/// node-to-node hop (ancestor expansion, for a click) is still resolved
/// lazily via the separate `/ancestors` endpoint, only when a reference is
/// actually clicked.
fn outgoing_reference_summaries(
    store: &SqliteStore,
    id: NodeId,
) -> Result<Vec<ReferenceSummary>, StoreError> {
    let references = store.outgoing_references(id)?;
    references
        .into_iter()
        .map(|reference| {
            let target = match reference.target {
                ReferenceTarget::Unresolved { target_ref } => {
                    ReferenceTargetSummary::Unresolved { target_ref }
                }
                ReferenceTarget::Resolved {
                    node_id,
                    target_ref,
                    ..
                } => match store.get(node_id)? {
                    Some(node) => {
                        let path = store
                            .canonical_path(node_id)?
                            .iter()
                            .map(mdtree_core::Slug::as_str)
                            .collect::<Vec<_>>()
                            .join("/");
                        ReferenceTargetSummary::Resolved {
                            node_id: node_id.to_string(),
                            title: node.fields().metadata.title.clone(),
                            path,
                        }
                    }
                    // The target node was deleted since this reference was
                    // recorded — degrade to unresolved rather than failing
                    // the whole node fetch over one dangling reference.
                    None => ReferenceTargetSummary::Unresolved {
                        target_ref: target_ref.unwrap_or_else(|| node_id.to_string()),
                    },
                },
            };
            Ok(ReferenceSummary {
                reference_type: reference.reference_type.as_str().to_string(),
                target,
            })
        })
        .collect()
}

fn resolve_selector(store: &SqliteStore, raw: &str) -> Result<NodeId, ApiError> {
    let selector = NodeSelector::from_str(raw).map_err(|_| ApiError::NotFound)?;
    store
        .resolve(&selector)?
        .map(|node| node.id())
        .ok_or(ApiError::NotFound)
}

pub(crate) async fn node(
    State(state): State<AppState>,
    Path((workspace, selector)): Path<(usize, String)>,
) -> Result<Json<NodeResponse>, ApiError> {
    let workspace = resolve_workspace(&state, workspace)?;
    let store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");
    let id = resolve_selector(&store, &selector)?;
    let node = store.get(id)?.ok_or(ApiError::NotFound)?;
    let children = store.children(id)?;

    let mut summaries = Vec::with_capacity(children.len());
    for child in &children {
        let child_count = store.children(child.id())?.len();
        summaries.push(summarize(
            child,
            store
                .canonical_path(child.id())?
                .iter()
                .map(mdtree_core::Slug::as_str)
                .collect::<Vec<_>>()
                .join("/"),
            u64::try_from(child_count).unwrap_or(u64::MAX),
            outgoing_reference_summaries(&store, child.id())?,
        ));
    }

    Ok(Json(NodeResponse {
        summary: summarize(
            &node,
            store
                .canonical_path(id)?
                .iter()
                .map(mdtree_core::Slug::as_str)
                .collect::<Vec<_>>()
                .join("/"),
            u64::try_from(children.len()).unwrap_or(u64::MAX),
            outgoing_reference_summaries(&store, id)?,
        ),
        children: summaries,
    }))
}

#[derive(Serialize)]
pub(crate) struct RenderResponse {
    html: String,
}

pub(crate) async fn render(
    State(state): State<AppState>,
    Path((workspace, selector)): Path<(usize, String)>,
) -> Result<Json<RenderResponse>, ApiError> {
    let workspace = resolve_workspace(&state, workspace)?;
    let store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");
    let id = resolve_selector(&store, &selector)?;
    let node = store.get(id)?.ok_or(ApiError::NotFound)?;

    Ok(Json(RenderResponse {
        html: render_sanitized_html(&node.fields().markdown_content),
    }))
}

#[derive(Serialize)]
pub(crate) struct SourceResponse {
    markdown_content: String,
    /// Optimistic-concurrency token for a subsequent `update_node` command,
    /// fetched fresh at the moment editing begins rather than trusting a
    /// possibly-stale `NodeSummary.version` the client cached earlier.
    version: u64,
}

pub(crate) async fn source(
    State(state): State<AppState>,
    Path((workspace, selector)): Path<(usize, String)>,
) -> Result<Json<SourceResponse>, ApiError> {
    let workspace = resolve_workspace(&state, workspace)?;
    let store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");
    let id = resolve_selector(&store, &selector)?;
    let node = store.get(id)?.ok_or(ApiError::NotFound)?;

    Ok(Json(SourceResponse {
        markdown_content: node.fields().markdown_content.clone(),
        version: node.fields().version,
    }))
}

#[derive(Serialize)]
pub(crate) struct AncestorsResponse {
    /// Root-to-parent ancestor IDs, oldest first — lets the client expand
    /// (and load, if not yet cached) each one to bring a reference's target
    /// into view, the same way search results already do (see
    /// `search::SearchResultItem::ancestor_ids`).
    ancestor_ids: Vec<String>,
}

pub(crate) async fn ancestors(
    State(state): State<AppState>,
    Path((workspace, selector)): Path<(usize, String)>,
) -> Result<Json<AncestorsResponse>, ApiError> {
    let workspace = resolve_workspace(&state, workspace)?;
    let store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");
    let id = resolve_selector(&store, &selector)?;
    let ancestor_ids = store
        .ancestors(id)?
        .into_iter()
        .map(|depth| depth.node.id().to_string())
        .collect();

    Ok(Json(AncestorsResponse { ancestor_ids }))
}
