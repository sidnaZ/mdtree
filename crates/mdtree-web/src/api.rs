//! Bounded read endpoints: session metadata, node children, and rendered
//! Markdown. Every read goes through the existing `SqliteStore` — no tree,
//! version, or path logic is duplicated here.

use std::str::FromStr;
use std::sync::atomic::Ordering;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use mdtree_core::{Node, NodeId, NodeMetadata, NodeSelector, ReferenceTarget, SemanticError};
use mdtree_sqlite::{checkpoint_workspace as checkpoint_store, SqliteStore, StoreError};
use serde::Serialize;

use crate::lifecycle::SESSION_HEADER;
use crate::markdown::render_sanitized_html;
use crate::state::AppState;

/// A small error for read-endpoint handlers.
pub(crate) enum ApiError {
    NotFound,
    Internal(StoreError),
    Semantic(SemanticError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "node not found").into_response(),
            ApiError::Internal(error) => {
                (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
            }
            ApiError::Semantic(error) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"status":"error","error":error})),
            )
                .into_response(),
        }
    }
}

impl From<SemanticError> for ApiError {
    fn from(error: SemanticError) -> Self {
        Self::Semantic(error)
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
    /// Every node type this workspace uses, either as a node's own
    /// `node_type` or as an entry in some node's `accepts_children` — lets
    /// the metadata editor suggest known types without requiring a node to
    /// be loaded first.
    node_types: Vec<String>,
    /// Whether all observed changes are merged into the primary database file.
    checkpoint_ready: bool,
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
            node_types: store.all_node_types()?,
            checkpoint_ready: workspace.checkpointed_revision.load(Ordering::SeqCst)
                == store.workspace_revision()?,
        });
    }
    Ok(Json(WorkspacesResponse {
        session_credential: state.session_credential.to_string(),
        server_version: env!("CARGO_PKG_VERSION"),
        workspaces,
    }))
}

/// Authenticates and checkpoints one workspace without stopping the web UI.
pub(crate) async fn checkpoint_workspace(
    State(state): State<AppState>,
    Path(workspace): Path<usize>,
    headers: HeaderMap,
) -> Response {
    let supplied = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok());
    if supplied != Some(&*state.session_credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(workspace) = resolve_workspace(&state, workspace) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let result = {
        let store = workspace
            .store
            .lock()
            .expect("workspace store mutex poisoned");
        // A writer in another process may commit immediately after the checkpoint.
        // Credit only the revision that was visible before this checkpoint began;
        // the revision poller will report any later commit as pending.
        let revision = store.workspace_revision();
        checkpoint_store(&store).and_then(|report| {
            let revision = revision?;
            Ok((report, revision))
        })
    };
    match result {
        Ok((report, revision)) => {
            if report.complete {
                workspace
                    .checkpointed_revision
                    .store(revision, Ordering::SeqCst);
            }
            Json(report).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
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
    let workspace_index = workspace;
    let workspace = resolve_workspace(&state, workspace)?;
    let store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");
    let id = resolve_selector(&store, &selector)?;
    let node = store.get(id)?.ok_or(ApiError::NotFound)?;

    Ok(Json(RenderResponse {
        html: render_sanitized_html(
            &node.fields().markdown_content,
            &format!("/api/{workspace_index}/asset/"),
        ),
    }))
}

#[derive(Serialize)]
pub(crate) struct SourceResponse {
    markdown_content: String,
    /// Seeds the metadata editor. Serializes the same way as the on-disk
    /// JSON (`extensions` flattened back to top-level keys), so a client can
    /// show it in Raw mode with no reshaping.
    metadata: NodeMetadata,
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
        metadata: node.fields().metadata.clone(),
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

const DOCX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

/// Exports the selected node and its whole subtree as a `.docx` download in
/// which every node is its own heading-led section (see `docx_export`).
pub(crate) async fn export_docx(
    State(state): State<AppState>,
    Path((workspace, selector)): Path<(usize, String)>,
) -> Result<Response, ApiError> {
    let workspace = resolve_workspace(&state, workspace)?;
    let (nodes, images) = {
        let store = workspace
            .store
            .lock()
            .expect("workspace store mutex poisoned");
        let id = resolve_selector(&store, &selector)?;
        crate::docx_export::read_export(&store, id)?
    };
    let document =
        tokio::task::spawn_blocking(move || crate::docx_export::build_docx(&nodes, &images))
            .await
            .map_err(|_| ApiError::NotFound)?;
    let file_name = export_file_name(&document.root_title);
    let mut response = Response::new(axum::body::Body::from(document.bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(DOCX_MEDIA_TYPE),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(&file_name))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=export.docx")),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// A bounded, separator-free download name derived from the export root title.
fn export_file_name(title: &str) -> String {
    let name: String = title
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                character
            }
        })
        .take(100)
        .collect();
    let name = name.trim().trim_matches('.');
    if name.is_empty() {
        "export.docx".to_owned()
    } else {
        format!("{name}.docx")
    }
}

/// `attachment` disposition with an ASCII fallback and an RFC 5987 UTF-8 name.
fn content_disposition(file_name: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(file_name.len() * 3);
    for byte in file_name.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(*byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("attachment; filename=export.docx; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::{content_disposition, export_file_name};

    #[test]
    fn export_file_names_are_separator_free_and_utf8_encoded() {
        assert_eq!(export_file_name("a/b: c"), "a_b_ c.docx");
        assert_eq!(export_file_name(" .. "), "export.docx");
        assert_eq!(
            content_disposition("Rokasgrāmata.docx"),
            "attachment; filename=export.docx; filename*=UTF-8''Rokasgr%C4%81mata.docx"
        );
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct AssetUploadQuery {
    /// Original file name, used to derive the asset name.
    name: Option<String>,
}

/// Serves one image asset with its stored media type. Content is untrusted:
/// `nosniff` and a no-script CSP keep it inert even if opened directly.
pub(crate) async fn asset(
    State(state): State<AppState>,
    Path((workspace, name)): Path<(usize, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let workspace = resolve_workspace(&state, workspace)?;
    let name: mdtree_core::AssetName = name.parse().map_err(|_| ApiError::NotFound)?;
    let (record, bytes) = {
        let store = workspace
            .store
            .lock()
            .expect("workspace store mutex poisoned");
        store.asset_bytes(&name)?.ok_or(ApiError::NotFound)?
    };
    let etag = format!(
        "\"{}\"",
        record.hash.as_bytes()[..16]
            .iter()
            .fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    );
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes())
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let mut response = Response::new(axum::body::Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(record.media_type.as_str()),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, etag);
    }
    Ok(response)
}

/// Stores an uploaded image (raw request body) as a new asset and returns
/// its record; an identical image already stored under the name is reused,
/// and a different one gets the next free `name-N`. Authenticated by the
/// per-launch session credential like every other write.
pub(crate) async fn upload_asset(
    State(state): State<AppState>,
    Path(workspace): Path<usize>,
    axum::extract::Query(query): axum::extract::Query<AssetUploadQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let supplied = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok());
    if supplied != Some(&*state.session_credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(workspace) = resolve_workspace(&state, workspace) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let info = match mdtree_core::inspect_image(&body) {
        Ok(info) => info,
        Err(error) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
        }
    };
    let name = mdtree_core::AssetName::from_file_name(
        query.name.as_deref().unwrap_or("image"),
        info.media_type,
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let result = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned")
        .put_asset(&name, &body, mdtree_sqlite::AssetWriteMode::Unique, now);
    match result {
        Ok(record) => (StatusCode::CREATED, Json(record)).into_response(),
        Err(StoreError::Asset(error)) => {
            (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}
