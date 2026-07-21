//! Read-only Model Context Protocol adapter for `MDTree`.

#![allow(clippy::missing_errors_doc)]
#![allow(missing_docs)]

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use mdtree_core::{
    BatchChildrenRequest, NodeId, NodeSelector, NodeType, PageCursor, PageLimit, PaginationError,
    ReferenceType, SearchFilters, SearchRequest, SearchScope, SystemUlidGenerator,
    DEFAULT_PAGE_LIMIT,
};
use mdtree_sqlite::{export_snapshot, workspace_status, SqliteStore};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, ServerCapabilities, ServerInfo,
};
use rmcp::schemars;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler};
use serde::{Deserialize, Serialize};

mod mutation;
mod switching;

pub use mutation::{
    ExportNodeParams, InitializeWorkspaceParams, McpAccessMode, MutationErrorDetail,
    MutationOptions, MutationResult, MutationStatus, WritePrecondition,
};
pub use switching::{SwitchWorkspaceParams, WorkspaceSwitchPolicy};

const MAX_ITEMS: u32 = 100;
const MAX_BYTES: usize = 1_048_576;
const MCP_INSTRUCTIONS: &str = "Use only tool names and schemas exposed by this server. Preserve the complete client-exposed function name: if tools are namespaced, invoke them through that namespace; never emit a bare name such as children or subtree. Never invent, concatenate, or infer tool names. For existing workspaces, call workspace_status first and verify its path. If that path does not match the requested workspace, call switch_workspace when exposed, then verify workspace_status again; use the CLI only when switching is unavailable or rejected. To list all nodes, pass its root_id as subtree's selector and follow every next_cursor until complete; use children for direct children and likewise follow pagination. The mdtree://tree and mdtree://references resources intentionally serialize complete whole-workspace collections and may be large; use bounded tools for targeted reads. For an uninitialized workspace, use initialize_workspace only when exposed and follow its schema. Do not claim a tool unavailable unless a real call returns that error. Never use the MDTree CLI when an equivalent MCP tool is exposed. ";

#[derive(Clone)]
pub struct MdtreeServer {
    binding: Arc<Mutex<WorkspaceBinding>>,
    tool_router: ToolRouter<Self>,
    access_mode: McpAccessMode,
    workspace_switch_policy: Option<WorkspaceSwitchPolicy>,
}

struct WorkspaceBinding {
    path: PathBuf,
    store: Option<SqliteStore>,
}

struct WorkspaceStoreGuard<'a>(MutexGuard<'a, WorkspaceBinding>);

impl Deref for WorkspaceStoreGuard<'_> {
    type Target = SqliteStore;

    fn deref(&self) -> &Self::Target {
        self.0
            .store
            .as_ref()
            .expect("workspace guard is initialized")
    }
}

impl DerefMut for WorkspaceStoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .store
            .as_mut()
            .expect("workspace guard is initialized")
    }
}

impl WorkspaceStoreGuard<'_> {
    fn path(&self) -> &Path {
        &self.0.path
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SelectorParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct BatchNodeParams {
    /// One to 100 selectors; order and duplicates are preserved.
    pub selectors: Vec<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct BatchChildrenRequestParams {
    pub parent: String,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct BatchChildrenParams {
    /// One to 20 grouped parent page requests; aggregate limits may not exceed 100.
    pub requests: Vec<BatchChildrenRequestParams>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct BoundedTreeParams {
    pub selector: String,
    #[serde(default = "default_depth")]
    pub depth: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct InspectionParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
    /// Maximum depth relative to the selected root.
    #[serde(default = "default_depth")]
    pub depth: u32,
    /// Maximum items in this page (1 through 100).
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Opaque continuation token returned by the preceding page.
    pub cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NavigationRelation {
    Parent,
    Children,
    Ancestors,
    Descendants,
    Siblings,
    Subtree,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct NavigationParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
    /// Exact structural relation to return. Omit only for legacy inspect-compatible behavior.
    pub relation: Option<NavigationRelation>,
    /// Maximum depth for the legacy inspect-compatible behavior.
    pub depth: Option<u32>,
    /// Maximum items for paginated relations (1 through 100).
    pub limit: Option<u32>,
    /// Opaque continuation token returned by the preceding relation page.
    pub cursor: Option<String>,
}

impl NavigationParams {
    fn validated_page(&self) -> Result<(PageLimit, Option<PageCursor>), PaginationError> {
        Ok((
            PageLimit::new(self.limit.unwrap_or(DEFAULT_PAGE_LIMIT))?,
            self.cursor.as_deref().map(str::parse).transpose()?,
        ))
    }

    fn reject_depth(&self) -> Result<(), ErrorData> {
        if self.depth.is_some() {
            return Err(invalid("depth is supported only when relation is omitted"));
        }
        Ok(())
    }

    fn reject_pagination(&self) -> Result<(), ErrorData> {
        if self.limit.is_some() || self.cursor.is_some() {
            return Err(invalid(
                "limit and cursor are not supported for parent or ancestors",
            ));
        }
        Ok(())
    }
}

impl InspectionParams {
    fn validated(&self) -> Result<(PageLimit, Option<PageCursor>), PaginationError> {
        Ok((
            PageLimit::new(self.limit)?,
            self.cursor.as_deref().map(str::parse).transpose()?,
        ))
    }
}

/// Reusable structured continuation fields for paginated MCP tools.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct PaginationParams {
    /// Maximum items in this page (1 through 100).
    #[serde(default = "default_page_limit")]
    pub limit: u32,
    /// Opaque continuation token returned by the preceding page.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct PaginatedSelectorParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
    /// Shared bounded-page and opaque-continuation fields.
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraversalOrder {
    #[default]
    Dfs,
    Bfs,
}

impl From<TraversalOrder> for mdtree_core::TraversalOrder {
    fn from(value: TraversalOrder) -> Self {
        match value {
            TraversalOrder::Dfs => Self::Dfs,
            TraversalOrder::Bfs => Self::Bfs,
        }
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct TraversalParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
    /// Database-side traversal order.
    #[serde(default)]
    pub order: TraversalOrder,
    /// Shared bounded-page and opaque-continuation fields.
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct VersionDiffParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
    /// Earlier retained node version.
    pub from: u64,
    /// Later retained node version.
    pub to: u64,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct RevisionParams {
    /// Stable ID, slug, or canonical path.
    pub selector: String,
    /// Exact retained immutable node version.
    pub version: u64,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ContainsParams {
    /// Proposed ancestor selected by stable ID, slug, or canonical path.
    pub ancestor: String,
    /// Proposed descendant selected by stable ID, slug, or canonical path.
    pub descendant: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct PairParams {
    /// First node selected by stable ID, slug, or canonical path.
    pub left: String,
    /// Second node selected by stable ID, slug, or canonical path.
    pub right: String,
}

#[derive(Clone, Copy, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterPredicate {
    Leaf,
    Internal,
}

impl From<FilterPredicate> for mdtree_core::StructuralPredicate {
    fn from(value: FilterPredicate) -> Self {
        match value {
            FilterPredicate::Leaf => Self::Leaf,
            FilterPredicate::Internal => Self::Internal,
        }
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct FilterNodesParams {
    /// Subtree root selected by stable ID, slug, or canonical path.
    pub selector: String,
    /// Match leaves or internal nodes.
    pub predicate: FilterPredicate,
    /// Optional maximum relative depth, including depth zero for the selected root.
    pub max_depth: Option<u32>,
    /// Shared bounded-page and opaque-continuation fields.
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ChildAtParams {
    /// Parent selected by stable ID, slug, or canonical path.
    pub parent: String,
    /// Zero-based canonical child position.
    pub index: u32,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SubtreeDiffParams {
    /// Root of the left/current subtree state.
    pub from_selector: String,
    /// Root of the right/current subtree state.
    pub to_selector: String,
    /// Shared bounded-page and opaque-continuation fields.
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

impl PaginationParams {
    /// Parses the shared limit and opaque cursor contracts.
    pub fn validated(&self) -> Result<(PageLimit, Option<PageCursor>), PaginationError> {
        Ok((
            PageLimit::new(self.limit)?,
            self.cursor.as_deref().map(str::parse).transpose()?,
        ))
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SearchParams {
    /// Free-text query.
    pub query: String,
    /// Structural scope; defaults to the complete workspace.
    pub scope: Option<String>,
    /// Stable ID, slug, or canonical path anchoring non-workspace scopes.
    pub scope_node: Option<String>,
    /// Eligible node types; multiple values are `ORed`.
    #[serde(default)]
    pub node_types: Vec<String>,
    /// Required tags; every value must be present.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Eligible outgoing status-reference types; multiple values are `ORed`.
    #[serde(default)]
    pub statuses: Vec<String>,
    pub min_depth: Option<u32>,
    pub max_depth: Option<u32>,
    pub created_from: Option<u64>,
    pub created_to: Option<u64>,
    pub updated_from: Option<u64>,
    pub updated_to: Option<u64>,
    pub structure: Option<FilterPredicate>,
    /// Shared bounded-page and opaque-continuation fields.
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct LocateParams {
    pub query: String,
    pub node_type: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ContextParams {
    pub selector: String,
    #[serde(default = "default_bytes")]
    pub byte_limit: usize,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ResolveReferenceParams {
    pub target: String,
}

const fn default_depth() -> u32 {
    2
}
const fn default_limit() -> u32 {
    20
}
const fn default_page_limit() -> u32 {
    DEFAULT_PAGE_LIMIT
}
const fn default_bytes() -> usize {
    16_384
}

impl MdtreeServer {
    pub fn open(path: &Path) -> Result<Self, mdtree_sqlite::WorkspaceError> {
        Self::open_with_mode(path, McpAccessMode::ReadOnly)
    }

    pub fn open_with_mode(
        path: &Path,
        access_mode: McpAccessMode,
    ) -> Result<Self, mdtree_sqlite::WorkspaceError> {
        Ok(Self::from_store(
            path,
            access_mode,
            Some(SqliteStore::open(path)?),
            None,
        ))
    }

    /// Opens an existing workspace, or starts an initialization-capable write server when the
    /// configured path does not exist yet.
    pub fn open_or_uninitialized_with_mode(
        path: &Path,
        access_mode: McpAccessMode,
    ) -> Result<Self, mdtree_sqlite::WorkspaceError> {
        let store = if !path.exists() && access_mode.allows_write() {
            None
        } else {
            Some(SqliteStore::open(path)?)
        };
        Ok(Self::from_store(path, access_mode, store, None))
    }

    /// Opens a server with an optional runtime workspace-switching policy.
    pub fn open_or_uninitialized_with_mode_and_policy(
        path: &Path,
        access_mode: McpAccessMode,
        workspace_switch_policy: Option<WorkspaceSwitchPolicy>,
    ) -> Result<Self, mdtree_sqlite::WorkspaceError> {
        let store = if !path.exists() && access_mode.allows_write() {
            None
        } else {
            Some(SqliteStore::open(path)?)
        };
        Ok(Self::from_store(
            path,
            access_mode,
            store,
            workspace_switch_policy,
        ))
    }

    fn from_store(
        path: &Path,
        access_mode: McpAccessMode,
        store: Option<SqliteStore>,
        workspace_switch_policy: Option<WorkspaceSwitchPolicy>,
    ) -> Self {
        let mut tool_router = Self::tool_router();
        if access_mode.allows_write() {
            tool_router.merge(Self::write_tool_router());
        }
        if workspace_switch_policy.is_some() {
            tool_router.merge(Self::switch_tool_router());
        }
        let path = absolute_workspace_path(path);
        Self {
            binding: Arc::new(Mutex::new(WorkspaceBinding { path, store })),
            tool_router,
            access_mode,
            workspace_switch_policy,
        }
    }

    fn store(&self) -> Result<WorkspaceStoreGuard<'_>, ErrorData> {
        let guard = self
            .binding
            .lock()
            .map_err(|_| mcp_error("workspace lock poisoned"))?;
        if guard.store.is_none() {
            return Err(invalid(
                "workspace is not initialized; call initialize_workspace first",
            ));
        }
        Ok(WorkspaceStoreGuard(guard))
    }

    fn store_and_id(&self, selector: &str) -> Result<(WorkspaceStoreGuard<'_>, NodeId), ErrorData> {
        let store = self.store()?;
        let id = Self::id_in(&store, selector)?;
        Ok((store, id))
    }

    fn id_in(store: &SqliteStore, selector: &str) -> Result<NodeId, ErrorData> {
        let selector =
            NodeSelector::from_str(selector).map_err(|error| invalid(error.to_string()))?;
        store
            .resolve(&selector)
            .map_err(store_error)?
            .map(|node| node.id())
            .ok_or_else(|| invalid("node not found"))
    }

    fn projection_in(
        store: &SqliteStore,
        selector: &str,
    ) -> Result<mdtree_core::NodeProjection, ErrorData> {
        let selector =
            NodeSelector::from_str(selector).map_err(|error| invalid(error.to_string()))?;
        store
            .resolve_projection(&selector)
            .map_err(store_error)?
            .ok_or_else(|| invalid("node not found"))
    }
}

fn absolute_workspace_path(path: &Path) -> PathBuf {
    if path.exists() {
        return path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_or_else(|_| path.to_path_buf(), |directory| directory.join(path))
    }
}

#[tool_router(router = tool_router)]
impl MdtreeServer {
    #[tool(description = "Return workspace format, root, counts, and index state")]
    async fn workspace_status(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        json_result(
            workspace_status(store.connection(), store.path())
                .map_err(|e| mcp_error(e.to_string()))?,
        )
    }

    #[tool(
        description = "Validate workspace integrity without mutation and return bounded findings"
    )]
    async fn validate(
        &self,
        Parameters(p): Parameters<PaginationParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let (limit, cursor) = p.validated().map_err(pagination_error)?;
        json_result(
            store
                .integrity_page(limit, cursor.as_ref())
                .map_err(page_read_error)?,
        )
    }

    #[tool(description = "Return the canonical root node")]
    async fn root(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        json_result(vec![store.root_projection().map_err(store_error)?])
    }

    #[tool(description = "Return one canonical node by ID, slug, or path")]
    async fn node(
        &self,
        Parameters(p): Parameters<SelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        json_result(vec![Self::projection_in(&store, &p.selector)?])
    }

    #[tool(description = "Resolve one to 100 selectors with ordered per-item results and errors")]
    async fn batch_nodes(
        &self,
        Parameters(p): Parameters<BatchNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        json_result(store.batch_node_lookup(&p.selectors).map_err(store_error)?)
    }

    #[tool(
        description = "Return grouped canonical child pages with per-parent cursors and aggregate bounds"
    )]
    async fn batch_children(
        &self,
        Parameters(p): Parameters<BatchChildrenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let requests = p
            .requests
            .into_iter()
            .map(|request| BatchChildrenRequest {
                parent: request.parent,
                limit: request.limit,
                cursor: request.cursor,
            })
            .collect::<Vec<_>>();
        let store = self.store()?;
        json_result(
            store
                .batch_children_lookup(&requests)
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Return database-side child, size, leaf, depth, and width statistics")]
    async fn statistics(
        &self,
        Parameters(p): Parameters<SelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(store.tree_statistics(id).map_err(store_error)?)
    }

    #[tool(description = "Test reflexive ancestor containment directly")]
    async fn contains(
        &self,
        Parameters(p): Parameters<ContainsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let ancestor = Self::id_in(&store, &p.ancestor)?;
        let descendant = Self::id_in(&store, &p.descendant)?;
        json_result(
            store
                .contains_ancestor(ancestor, descendant)
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Return the reflexive lowest common ancestor of two nodes")]
    async fn lowest_common_ancestor(
        &self,
        Parameters(p): Parameters<PairParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let left = Self::id_in(&store, &p.left)?;
        let right = Self::id_in(&store, &p.right)?;
        json_result(
            store
                .lowest_common_ancestor(left, right)
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Return the endpoint-inclusive canonical path between two nodes")]
    async fn path_between(
        &self,
        Parameters(p): Parameters<PairParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let left = Self::id_in(&store, &p.left)?;
        let right = Self::id_in(&store, &p.right)?;
        json_result(store.path_between(left, right).map_err(store_error)?)
    }

    #[tool(description = "Return the canonical edge distance between two nodes")]
    async fn distance(
        &self,
        Parameters(p): Parameters<PairParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let left = Self::id_in(&store, &p.left)?;
        let right = Self::id_in(&store, &p.right)?;
        json_result(store.tree_distance(left, right).map_err(store_error)?)
    }

    #[tool(description = "Return a resumable DFS page of leaf or internal subtree nodes")]
    async fn filter_nodes(
        &self,
        Parameters(p): Parameters<FilterNodesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .filtered_subtree_page(
                    id,
                    p.predicate.into(),
                    p.max_depth,
                    candidate,
                    cursor.as_ref(),
                )
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Return the zero-based canonical child at one index")]
    async fn child_at(
        &self,
        Parameters(p): Parameters<ChildAtParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let parent = Self::id_in(&store, &p.parent)?;
        json_result(store.child_at(parent, p.index).map_err(store_error)?)
    }

    #[tool(description = "Return direct previous and next canonical siblings")]
    async fn adjacent_siblings(
        &self,
        Parameters(p): Parameters<SelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(store.adjacent_siblings(id).map_err(store_error)?)
    }

    #[tool(
        description = "Return a resumable page of direct children in deterministic sibling order"
    )]
    async fn children(
        &self,
        Parameters(p): Parameters<PaginatedSelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .children_page(id, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Return the direct canonical parent")]
    async fn parent(
        &self,
        Parameters(p): Parameters<SelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let parent = store.parent_projection(id).map_err(store_error)?;
        json_result(parent.into_iter().collect::<Vec<_>>())
    }

    #[tool(description = "Return root-to-parent ancestors")]
    async fn ancestors(
        &self,
        Parameters(p): Parameters<SelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(depth_nodes(
            &store,
            store.ancestors(id).map_err(store_error)?,
        ))
    }

    #[tool(description = "Return a resumable DFS or BFS page of descendants with relative depth")]
    async fn descendants(
        &self,
        Parameters(p): Parameters<TraversalParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .descendants_page_ordered(id, p.order.into(), candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Return a resumable page of nodes sharing the same canonical parent")]
    async fn siblings(
        &self,
        Parameters(p): Parameters<PaginatedSelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .siblings_page(id, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(
        description = "Return a resumable DFS or BFS page containing a node and descendants with relative depth"
    )]
    async fn subtree(
        &self,
        Parameters(p): Parameters<TraversalParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .subtree_page_ordered(id, p.order.into(), candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Return retained node revisions newest-first as a resumable page")]
    async fn history(
        &self,
        Parameters(p): Parameters<PaginatedSelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .revision_history_page(id, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Return one exact retained node revision without changing the head")]
    async fn revision(
        &self,
        Parameters(p): Parameters<RevisionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(
            store
                .revision(id, p.version)
                .map_err(store_error)?
                .ok_or_else(|| invalid(format!("version {} not found", p.version)))?,
        )
    }

    #[tool(description = "Compare any two retained versions of one node without mutation")]
    async fn version_diff(
        &self,
        Parameters(p): Parameters<VersionDiffParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let from = store
            .revision(id, p.from)
            .map_err(store_error)?
            .ok_or_else(|| invalid(format!("version {} not found", p.from)))?;
        let to = store
            .revision(id, p.to)
            .map_err(store_error)?
            .ok_or_else(|| invalid(format!("version {} not found", p.to)))?;
        json_result(mdtree_core::diff_revisions(&from, &to))
    }

    #[tool(
        description = "Return a resumable bounded structural/content comparison of two current subtrees"
    )]
    async fn subtree_diff(
        &self,
        Parameters(p): Parameters<SubtreeDiffParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let from = Self::id_in(&store, &p.from_selector)?;
        let to = Self::id_in(&store, &p.to_selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .subtree_diff_page(from, to, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(
        description = "Select parent, children, ancestors, descendants, siblings, or subtree; omitting relation preserves the legacy bounded-inspection response"
    )]
    async fn navigate(
        &self,
        Parameters(p): Parameters<NavigationParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let Some(relation) = p.relation else {
            let (limit, cursor) = p.validated_page().map_err(pagination_error)?;
            return byte_bounded_page_result(limit, |candidate| {
                store
                    .inspect_subtree_page(
                        "navigate",
                        id,
                        p.depth.unwrap_or(default_depth()),
                        candidate,
                        cursor.as_ref(),
                    )
                    .map_err(page_read_error)
            });
        };
        p.reject_depth()?;
        match relation {
            NavigationRelation::Parent => {
                p.reject_pagination()?;
                let parent = store.parent_projection(id).map_err(store_error)?;
                json_result(parent.into_iter().collect::<Vec<_>>())
            }
            NavigationRelation::Ancestors => {
                p.reject_pagination()?;
                json_result(depth_nodes(
                    &store,
                    store.ancestors(id).map_err(store_error)?,
                ))
            }
            NavigationRelation::Children => {
                let (limit, cursor) = p.validated_page().map_err(pagination_error)?;
                byte_bounded_page_result(limit, |candidate| {
                    store
                        .children_page(id, candidate, cursor.as_ref())
                        .map_err(page_read_error)
                })
            }
            NavigationRelation::Descendants => {
                let (limit, cursor) = p.validated_page().map_err(pagination_error)?;
                byte_bounded_page_result(limit, |candidate| {
                    store
                        .descendants_page(id, candidate, cursor.as_ref())
                        .map_err(page_read_error)
                })
            }
            NavigationRelation::Siblings => {
                let (limit, cursor) = p.validated_page().map_err(pagination_error)?;
                byte_bounded_page_result(limit, |candidate| {
                    store
                        .siblings_page(id, candidate, cursor.as_ref())
                        .map_err(page_read_error)
                })
            }
            NavigationRelation::Subtree => {
                let (limit, cursor) = p.validated_page().map_err(pagination_error)?;
                byte_bounded_page_result(limit, |candidate| {
                    store
                        .subtree_page(id, candidate, cursor.as_ref())
                        .map_err(page_read_error)
                })
            }
        }
    }

    #[tool(description = "Return the canonical path and breadcrumb")]
    async fn path(
        &self,
        Parameters(p): Parameters<SelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(
            serde_json::json!({"node_id":id,"path":store.canonical_path(id).map_err(store_error)?,"breadcrumb":store.breadcrumb(id).map_err(store_error)?}),
        )
    }

    #[tool(description = "Search section-oriented content across the workspace")]
    async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let scope = match p.scope.as_deref().unwrap_or("workspace") {
            "current_node" => SearchScope::CurrentNode,
            "subtree" => SearchScope::Subtree,
            "siblings" => SearchScope::Siblings,
            "parent_subtree" => SearchScope::ParentSubtree,
            "workspace" => SearchScope::Workspace,
            "linked" => SearchScope::Linked,
            value => return Err(invalid(format!("unknown search scope {value}"))),
        };
        let store = self.store()?;
        let scope_node = p
            .scope_node
            .as_deref()
            .map(|selector| {
                let selector =
                    NodeSelector::from_str(selector).map_err(|error| invalid(error.to_string()))?;
                store
                    .resolve(&selector)
                    .map_err(store_error)?
                    .map(|node| node.id())
                    .ok_or_else(|| invalid("node not found"))
            })
            .transpose()?;
        if scope != SearchScope::Workspace && scope_node.is_none() {
            return Err(invalid("scope_node is required outside workspace scope"));
        }
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        let node_types = p
            .node_types
            .iter()
            .map(|value| NodeType::from_str(value).map_err(|error| invalid(error.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let statuses = p
            .statuses
            .iter()
            .map(|value| ReferenceType::from_str(value).map_err(|error| invalid(error.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let filters = SearchFilters {
            node_types,
            tags: p.tags,
            statuses,
            min_depth: p.min_depth,
            max_depth: p.max_depth,
            created_from: p.created_from,
            created_to: p.created_to,
            updated_from: p.updated_from,
            updated_to: p.updated_to,
            structure: p.structure.map(Into::into),
        };
        filters.validate(scope).map_err(invalid)?;
        let request = SearchRequest {
            query: p.query,
            scope,
            scope_node,
            filters,
            limit: limit.get(),
            offset: 0,
            prefix_last_token: true,
        };
        byte_bounded_page_result(limit, |candidate| {
            let mut candidate_request = request.clone();
            candidate_request.limit = candidate.get();
            store
                .search_content_page(&candidate_request, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Locate the best structural destination with confidence and alternatives")]
    async fn locate(
        &self,
        Parameters(p): Parameters<LocateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let kind = p
            .node_type
            .as_deref()
            .map(NodeType::from_str)
            .transpose()
            .map_err(|e| invalid(e.to_string()))?;
        json_result(
            self.store()?
                .locate_target(&p.query, kind.as_ref())
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Inspect a resumable bounded subtree page")]
    async fn inspect(
        &self,
        Parameters(p): Parameters<InspectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .inspect_subtree_page("inspect", id, p.depth, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Find nearby structurally similar examples")]
    async fn examples(
        &self,
        Parameters(p): Parameters<BoundedTreeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let node = store
            .get(id)
            .map_err(store_error)?
            .ok_or_else(|| invalid("node not found"))?;
        json_result(
            store
                .examples_for(
                    id,
                    node.fields().metadata.node_type.as_ref(),
                    p.limit.min(MAX_ITEMS),
                )
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Assemble bounded read context with deterministic truncation")]
    async fn read_context(
        &self,
        Parameters(p): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(
            store
                .read_context(id, p.byte_limit.min(MAX_BYTES))
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Assemble bounded optimistic-write context without mutating")]
    async fn write_context(
        &self,
        Parameters(p): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        json_result(
            store
                .write_context(id, p.byte_limit.min(MAX_BYTES))
                .map_err(store_error)?,
        )
    }

    #[tool(description = "Return outgoing typed references")]
    async fn references(
        &self,
        Parameters(p): Parameters<PaginatedSelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .outgoing_references_page(id, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Return incoming typed backlinks")]
    async fn backlinks(
        &self,
        Parameters(p): Parameters<PaginatedSelectorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (store, id) = self.store_and_id(&p.selector)?;
        let (limit, cursor) = p.pagination.validated().map_err(pagination_error)?;
        byte_bounded_page_result(limit, |candidate| {
            store
                .backlinks_page(id, candidate, cursor.as_ref())
                .map_err(page_read_error)
        })
    }

    #[tool(description = "Resolve a reference target without mutation")]
    async fn resolve_reference(
        &self,
        Parameters(p): Parameters<ResolveReferenceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        json_result(
            self.store()?
                .resolve_reference(&p.target)
                .map_err(store_error)?,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MdtreeServer {
    fn get_info(&self) -> ServerInfo {
        let mode_instructions = if self.access_mode.allows_write() {
            "MDTree read/write server. Mutation tools require optimistic concurrency; export_node may create Markdown files on the server filesystem."
        } else {
            "Read-only MDTree server. Workspace and filesystem write tools are not registered; restart with --allow-write to opt in."
        };
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(format!("{MCP_INSTRUCTIONS}{mode_instructions}"))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("mdtree://workspace", "workspace")
                .with_description("Small workspace status and format summary.")
                .with_mime_type("application/json"),
            Resource::new("mdtree://tree", "tree")
                .with_description(
                    "Complete whole-workspace canonical node collection; use paginated tree tools for targeted reads.",
                )
                .with_mime_type("application/json"),
            Resource::new("mdtree://references", "references")
                .with_description(
                    "Complete whole-workspace reference collection; use references or backlinks for targeted reads.",
                )
                .with_mime_type("application/json"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("mdtree://node/{node_id}", "node")
                .with_mime_type("application/json"),
            ResourceTemplate::new("mdtree://node/{node_id}/section/{anchor}", "node-section")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let uri = request.uri;
        let store = self.store()?;
        let value = if uri == "mdtree://workspace" {
            serde_json::to_value(
                workspace_status(store.connection(), store.path())
                    .map_err(|e| mcp_error(e.to_string()))?,
            )
            .map_err(json_error)?
        } else if uri == "mdtree://tree" {
            complete_workspace_resource(&store, CompleteResource::Tree)?
        } else if uri == "mdtree://references" {
            complete_workspace_resource(&store, CompleteResource::References)?
        } else if let Some(selector) = uri.strip_prefix("mdtree://node/") {
            if let Some((node_id, anchor)) = selector.split_once("/section/") {
                let id = NodeId::from_str(node_id).map_err(|e| invalid(e.to_string()))?;
                let node = store
                    .get(id)
                    .map_err(store_error)?
                    .ok_or_else(|| ErrorData::resource_not_found("node not found", None))?;
                let section = mdtree_markdown::parse_sections(
                    id,
                    &node.fields().markdown_content,
                    &SystemUlidGenerator,
                )
                .map_err(|e| mcp_error(e.to_string()))?
                .into_iter()
                .find(|section| section.anchor.as_deref() == Some(anchor))
                .ok_or_else(|| ErrorData::resource_not_found("section not found", None))?;
                serde_json::json!({"node_id":id,"heading":section.heading,"level":section.heading_level,"anchor":section.anchor,"content":section.content,"position":section.position})
            } else {
                let id = NodeId::from_str(selector).map_err(|e| invalid(e.to_string()))?;
                serde_json::to_value(snapshot_nodes(&store, &[id])?).map_err(json_error)?
            }
        } else {
            return Err(ErrorData::resource_not_found("resource not found", None));
        };
        let text = serde_json::to_string(&value).map_err(json_error)?;
        if text.len() > MAX_BYTES {
            return Err(invalid("resource exceeds 1048576-byte limit"));
        }
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )
        .with_mime_type("application/json")]))
    }
}

#[derive(Clone, Copy)]
enum CompleteResource {
    Tree,
    References,
}

/// The only MCP boundary allowed to use complete snapshot projection.
///
/// These two resources are explicit whole-workspace collection views. Targeted
/// tools and resource templates must use storage projections instead.
fn complete_workspace_resource(
    store: &SqliteStore,
    resource: CompleteResource,
) -> Result<serde_json::Value, ErrorData> {
    let snapshot = export_snapshot(store).map_err(|error| mcp_error(error.to_string()))?;
    match resource {
        CompleteResource::Tree => serde_json::to_value(snapshot.nodes).map_err(json_error),
        CompleteResource::References => {
            serde_json::to_value(snapshot.references).map_err(json_error)
        }
    }
}

fn snapshot_nodes(
    store: &SqliteStore,
    ids: &[NodeId],
) -> Result<Vec<mdtree_core::SnapshotNode>, ErrorData> {
    store.project_nodes(ids).map_err(store_error)
}

fn depth_nodes(
    store: &SqliteStore,
    depths: Vec<mdtree_sqlite::NodeDepth>,
) -> Vec<serde_json::Value> {
    store
        .project_depths(depths)
        .into_iter()
        .map(|item| serde_json::json!({"depth":item.depth,"node":item.node}))
        .collect()
}

fn json_result(value: impl Serialize) -> Result<CallToolResult, ErrorData> {
    let value = serde_json::to_value(value).map_err(json_error)?;
    json_value_result(value)
}

fn byte_bounded_page_result<T, F>(
    requested: PageLimit,
    mut load: F,
) -> Result<CallToolResult, ErrorData>
where
    T: Serialize,
    F: FnMut(PageLimit) -> Result<T, ErrorData>,
{
    for item_count in (1..=requested.get()).rev() {
        let candidate = PageLimit::new(item_count).map_err(pagination_error)?;
        let value = serde_json::to_value(load(candidate)?).map_err(json_error)?;
        if serde_json::to_vec(&value).map_err(json_error)?.len() <= MAX_BYTES {
            return json_value_result(value);
        }
    }
    Err(ErrorData::invalid_params(
        "single page item exceeds 1048576-byte response limit",
        Some(serde_json::json!({"code":"item_too_large","max_bytes":MAX_BYTES})),
    ))
}

fn json_value_result(value: serde_json::Value) -> Result<CallToolResult, ErrorData> {
    let bytes = serde_json::to_vec(&value).map_err(json_error)?;
    if bytes.len() > MAX_BYTES {
        return Err(invalid("response exceeds 1048576-byte limit"));
    }
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

#[allow(clippy::needless_pass_by_value)]
fn store_error(error: mdtree_sqlite::StoreError) -> ErrorData {
    let detail = MutationErrorDetail::from(&error);
    ErrorData::invalid_params(error.to_string(), serde_json::to_value(detail).ok())
}
fn page_read_error(error: mdtree_sqlite::PageReadError) -> ErrorData {
    match error {
        mdtree_sqlite::PageReadError::Store(error) => store_error(error),
        mdtree_sqlite::PageReadError::Pagination(error) => pagination_error(error),
    }
}
#[allow(clippy::needless_pass_by_value)]
fn pagination_error(error: PaginationError) -> ErrorData {
    ErrorData::invalid_params(
        error.to_string(),
        Some(serde_json::json!({"code": error.code()})),
    )
}
#[allow(clippy::needless_pass_by_value)]
fn json_error(error: serde_json::Error) -> ErrorData {
    mcp_error(error.to_string())
}
fn invalid(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}
fn mcp_error(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(message.into(), None)
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
    use rmcp::ServerHandler;
    use rmcp::ServiceExt;
    use tempfile::tempdir;

    use super::{
        complete_workspace_resource, snapshot_nodes, CompleteResource, McpAccessMode, MdtreeServer,
        PaginationParams,
    };

    fn server() -> (tempfile::TempDir, MdtreeServer) {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("northstar.mdtree");
        let bytes = include_bytes!("../../../examples/northstar-platform.snapshot.json");
        let plan = mdtree_sqlite::plan_json_import(bytes).expect("snapshot");
        mdtree_sqlite::import_snapshot_new(&path, &plan.snapshot).expect("import");
        let server = MdtreeServer::open(&path).expect("server");
        (directory, server)
    }

    #[test]
    fn mcp_pagination_fields_use_the_shared_opaque_contract() {
        let root = "01JZ8Q5CWPN8T7KPN5A1V9B6XM"
            .parse::<mdtree_core::NodeId>()
            .expect("root ID");
        let item = "01JZ8Q5CWPN8T7KPN5A1V9B6XN"
            .parse::<mdtree_core::NodeId>()
            .expect("item ID");
        let scope =
            mdtree_core::CursorScope::new("children", Some(root), "parent=root").expect("scope");
        let cursor = mdtree_core::PageCursor::issue(
            9,
            scope.clone(),
            mdtree_core::PagePosition::Sibling {
                sibling_order: 1,
                node_id: item,
            },
        )
        .expect("cursor");
        let params: PaginationParams = serde_json::from_value(serde_json::json!({
            "limit":2,
            "cursor":cursor
        }))
        .expect("structured params");
        let (limit, parsed) = params.validated().expect("validated params");
        assert_eq!(limit.get(), 2);
        assert_eq!(
            parsed.expect("cursor").resume(&scope, 9),
            Ok(mdtree_core::PagePosition::Sibling {
                sibling_order: 1,
                node_id: item,
            })
        );
        assert!(
            serde_json::from_value::<PaginationParams>(serde_json::json!({
                "limit":2,
                "cursor":"tampered"
            }))
            .expect("params")
            .validated()
            .is_err()
        );
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(PaginationParams))
            .expect("pagination schema")
            .to_string();
        assert!(schema.contains("limit"));
        assert!(schema.contains("cursor"));
        assert!(!schema.contains("unknown"));
    }

    #[test]
    fn mcp_node_and_depth_helpers_consume_shared_projection_contracts() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("projection.mdtree");
        let fixture = mdtree_core::generate_large_tree_fixture(
            mdtree_core::LargeTreeFixtureSpec {
                wide_children: 8,
                deep_descendants: 5,
                history_revisions: 4,
                relations: 6,
                response_boundary_bytes: 4096,
            },
            94,
        );
        mdtree_sqlite::import_snapshot_new(&path, &fixture.snapshot).expect("scale import");
        let store = mdtree_sqlite::SqliteStore::open(&path).expect("store");
        let requested = [fixture.wide_child_ids[4], fixture.history_node_id];

        let projected = super::snapshot_nodes(&store, &requested).expect("node projections");
        assert_eq!(
            projected.iter().map(|node| node.id).collect::<Vec<_>>(),
            requested
        );
        let depths = store
            .descendants(fixture.deep_parent_id)
            .expect("depth rows");
        let projected_depths = super::depth_nodes(&store, depths);
        assert_eq!(projected_depths[0]["depth"], 1);
        assert_eq!(
            projected_depths[0]["node"]["parent_id"],
            serde_json::json!(fixture.deep_parent_id)
        );
        assert!(projected_depths[0]["node"].get("content_hash").is_some());
    }

    #[test]
    fn complete_export_is_confined_to_explicit_whole_workspace_resources() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("resource-boundary.mdtree");
        let fixture = mdtree_core::generate_large_tree_fixture(
            mdtree_core::LargeTreeFixtureSpec {
                wide_children: 12,
                deep_descendants: 8,
                history_revisions: 6,
                relations: 10,
                response_boundary_bytes: 4096,
            },
            944,
        );
        mdtree_sqlite::import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let (store, observer) =
            mdtree_sqlite::test_support::open_observed_store(&path).expect("observed store");

        snapshot_nodes(&store, &[fixture.history_node_id]).expect("targeted node resource");
        assert!(!observer.observation().has_complete_snapshot_signature());

        for resource in [CompleteResource::Tree, CompleteResource::References] {
            observer.reset();
            complete_workspace_resource(&store, resource).expect("complete resource");
            assert!(observer.observation().has_complete_snapshot_signature());
        }

        let production = include_str!("lib.rs")
            .split_once("#[cfg(test)]")
            .expect("test module boundary")
            .0;
        assert_eq!(production.matches("export_snapshot(store)").count(), 1);
        assert_eq!(
            production
                .matches("complete_workspace_resource(&store")
                .count(),
            2,
            "only the tree and references resources may request complete projection"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn protocol_initializes_lists_read_only_tools_and_reads_resources() {
        let (_directory, server) = server();
        let (server_transport, client_transport) = tokio::io::duplex(65_536);
        let task = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .expect("serve")
                .waiting()
                .await
                .expect("wait");
        });
        let client = ().serve(client_transport).await.expect("initialize");
        let tools = client.peer().list_all_tools().await.expect("tools");
        let names = tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        for required in [
            "root",
            "node",
            "batch_nodes",
            "batch_children",
            "statistics",
            "contains",
            "lowest_common_ancestor",
            "path_between",
            "distance",
            "filter_nodes",
            "child_at",
            "adjacent_siblings",
            "children",
            "parent",
            "ancestors",
            "descendants",
            "siblings",
            "subtree",
            "history",
            "revision",
            "version_diff",
            "subtree_diff",
            "path",
            "search",
            "locate",
            "inspect",
            "examples",
            "read_context",
            "write_context",
            "references",
            "backlinks",
            "resolve_reference",
            "workspace_status",
            "validate",
        ] {
            assert!(names.contains(&required), "missing {required}");
        }
        for paginated in [
            "children",
            "siblings",
            "descendants",
            "subtree",
            "history",
            "filter_nodes",
            "inspect",
            "navigate",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == paginated)
                .expect("paginated tool");
            let schema = serde_json::to_string(&tool.input_schema).expect("input schema");
            for field in ["selector", "limit", "cursor"] {
                assert!(schema.contains(field), "{paginated} schema lacks {field}");
            }
        }
        let navigate_schema = serde_json::to_string(
            &tools
                .iter()
                .find(|tool| tool.name == "navigate")
                .expect("navigate tool")
                .input_schema,
        )
        .expect("navigate schema");
        let subtree_diff_schema = serde_json::to_string(
            &tools
                .iter()
                .find(|tool| tool.name == "subtree_diff")
                .expect("subtree diff tool")
                .input_schema,
        )
        .expect("subtree diff schema");
        for field in ["from_selector", "to_selector", "limit", "cursor"] {
            assert!(
                subtree_diff_schema.contains(field),
                "subtree diff schema lacks {field}"
            );
        }
        for relation in [
            "parent",
            "children",
            "ancestors",
            "descendants",
            "siblings",
            "subtree",
        ] {
            assert!(
                navigate_schema.contains(relation),
                "navigate schema lacks {relation}"
            );
        }
        assert!(!names
            .iter()
            .any(|name| ["create", "update", "move", "rename", "remove"].contains(name)));
        let resources = client.peer().list_resources(None).await.expect("resources");
        assert_eq!(resources.resources.len(), 3);
        let tree_resource = resources
            .resources
            .iter()
            .find(|resource| resource.uri == "mdtree://tree")
            .expect("tree resource");
        assert!(tree_resource
            .description
            .as_deref()
            .is_some_and(|description| description.contains("Complete whole-workspace")));
        let reference_resource = resources
            .resources
            .iter()
            .find(|resource| resource.uri == "mdtree://references")
            .expect("references resource");
        assert!(reference_resource
            .description
            .as_deref()
            .is_some_and(|description| description.contains("Complete whole-workspace")));
        let templates = client
            .peer()
            .list_resource_templates(None)
            .await
            .expect("templates");
        assert_eq!(templates.resource_templates.len(), 2);
        let workspace = client
            .peer()
            .read_resource(ReadResourceRequestParams::new("mdtree://workspace"))
            .await
            .expect("workspace resource");
        assert_eq!(workspace.contents.len(), 1);
        let section = client
            .peer()
            .read_resource(ReadResourceRequestParams::new(
                "mdtree://node/01JZ8Q5CWPN8T7KPN5A1V9B6XM/section/northstar-platform",
            ))
            .await
            .expect("section resource");
        assert_eq!(section.contents.len(), 1);
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("root"))
            .await
            .expect("root tool");
        assert!(!result.content.is_empty());
        let root_id = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
        let tool_cases = vec![
            ("workspace_status", serde_json::json!({})),
            ("node", serde_json::json!({"selector":root_id})),
            ("batch_nodes", serde_json::json!({"selectors":[root_id]})),
            (
                "batch_children",
                serde_json::json!({"requests":[{"parent":root_id,"limit":1}]}),
            ),
            ("children", serde_json::json!({"selector":root_id})),
            ("parent", serde_json::json!({"selector":root_id})),
            ("ancestors", serde_json::json!({"selector":root_id})),
            ("descendants", serde_json::json!({"selector":root_id})),
            ("siblings", serde_json::json!({"selector":root_id})),
            ("subtree", serde_json::json!({"selector":root_id})),
            (
                "version_diff",
                serde_json::json!({"selector":root_id,"from":1,"to":1}),
            ),
            (
                "subtree_diff",
                serde_json::json!({
                    "from_selector":root_id,
                    "to_selector":"01JZ8Q5CWPN8T7KPN5A1V9B6XN",
                    "limit":2
                }),
            ),
            (
                "navigate",
                serde_json::json!({"selector":root_id,"depth":2,"limit":20}),
            ),
            ("path", serde_json::json!({"selector":root_id})),
            (
                "search",
                serde_json::json!({"query":"domain events kafka","scope":"workspace","limit":10}),
            ),
            (
                "locate",
                serde_json::json!({"query":"Add architecture decision for API retries","node_type":"architecture_decision"}),
            ),
            (
                "inspect",
                serde_json::json!({"selector":root_id,"depth":2,"limit":20}),
            ),
            (
                "examples",
                serde_json::json!({"selector":"01JZ8Q5CWPN8T7KPN5A1V9B6XS","limit":3}),
            ),
            (
                "read_context",
                serde_json::json!({"selector":root_id,"byte_limit":8192}),
            ),
            (
                "write_context",
                serde_json::json!({"selector":root_id,"byte_limit":8192}),
            ),
            (
                "references",
                serde_json::json!({"selector":"01JZ8Q5CWPN8T7KPN5A1V9B6XX"}),
            ),
            (
                "backlinks",
                serde_json::json!({"selector":"01JZ8Q5CWPN8T7KPN5A1V9B6XS"}),
            ),
            ("resolve_reference", serde_json::json!({"target":"Orders"})),
        ];
        for (name, arguments) in tool_cases {
            let response = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new(name)
                        .with_arguments(arguments.as_object().expect("object").clone()),
                )
                .await
                .expect("tool response");
            assert_ne!(response.is_error, Some(true), "{name} failed");
            assert!(!response.content.is_empty(), "{name} was empty");
        }
        let invalid = client
            .peer()
            .call_tool(CallToolRequestParams::new("node"))
            .await
            .expect("protocol response");
        assert_eq!(invalid.is_error, Some(true));
        client.cancel().await.expect("cancel");
        task.await.expect("server task");
    }

    #[test]
    fn schemas_are_compact_and_every_response_has_a_hard_limit() {
        let (_directory, server) = server();
        let tools = server.tool_router.list_all();
        assert!(tools
            .iter()
            .all(|tool| serde_json::to_vec(&tool.input_schema)
                .expect("schema")
                .len()
                < 2_048));
        assert_eq!(super::MAX_BYTES, 1_048_576);
        assert_eq!(super::MAX_ITEMS, 100);
    }

    #[test]
    fn routine_tool_schemas_are_concrete_and_mutation_contracts_are_discoverable() {
        let (directory, _read_only) = server();
        let path = directory.path().join("northstar.mdtree");
        let read_write =
            MdtreeServer::open_with_mode(&path, McpAccessMode::ReadWrite).expect("write server");
        let tools = read_write.tool_router.list_all();

        for tool in &tools {
            assert!(
                tool.description
                    .as_deref()
                    .is_some_and(|description| !description.trim().is_empty()),
                "{} lacks a description",
                tool.name
            );
            let schema = serde_json::to_value(&tool.input_schema).expect("tool input schema");
            assert!(
                schema.is_object(),
                "{} has an opaque root schema",
                tool.name
            );
            assert_eq!(
                schema.get("type").and_then(serde_json::Value::as_str),
                Some("object"),
                "{} is not an object input schema",
                tool.name
            );
            assert!(
                schema
                    .get("properties")
                    .is_some_and(serde_json::Value::is_object),
                "{} lacks concrete input properties",
                tool.name
            );
        }

        let mutation_batch = tools
            .iter()
            .find(|tool| tool.name == "mutation_batch")
            .expect("mutation_batch tool");
        let schema = serde_json::to_string(&mutation_batch.input_schema).expect("batch schema");
        for operation in [
            "create",
            "update",
            "rename",
            "move",
            "reorder",
            "remove",
            "set_references",
        ] {
            assert!(schema.contains(operation), "batch schema lacks {operation}");
        }
        for field in ["operation_id", "operations", "options", "precondition"] {
            assert!(schema.contains(field), "batch schema lacks {field}");
        }
    }

    #[test]
    fn mutation_router_is_structurally_opt_in() {
        let (directory, read_only) = server();
        let path = directory.path().join("northstar.mdtree");
        let read_write =
            MdtreeServer::open_with_mode(&path, McpAccessMode::ReadWrite).expect("write server");
        let read_names = read_only
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        let write_names = read_write
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        assert!(!read_names
            .iter()
            .any(|name| name == "mutation_capabilities"));
        assert!(!read_names.iter().any(|name| name == "switch_workspace"));
        assert!(!write_names.iter().any(|name| name == "switch_workspace"));
        assert!(write_names
            .iter()
            .any(|name| name == "mutation_capabilities"));
        assert!(write_names
            .iter()
            .any(|name| name == "initialize_workspace"));
        assert!(write_names.iter().any(|name| name == "create_node"));
        assert!(write_names.iter().any(|name| name == "update_node"));
        assert!(write_names.iter().any(|name| name == "rename_node"));
        assert!(write_names.iter().any(|name| name == "move_node"));
        assert!(write_names.iter().any(|name| name == "clone_subtree"));
        assert!(write_names.iter().any(|name| name == "atomic_tree_batch"));
        assert!(write_names.iter().any(|name| name == "mutation_batch"));
        assert!(write_names.iter().any(|name| name == "reorder_node"));
        assert!(write_names.iter().any(|name| name == "export_node"));
        assert!(!read_names.iter().any(|name| name == "export_node"));
        for name in ["remove_node", "set_references", "restore_version"] {
            assert!(write_names.iter().any(|candidate| candidate == name));
            assert!(!read_names.iter().any(|candidate| candidate == name));
        }
        assert!(!read_names.iter().any(|name| name == "create_node"));
        assert!(!read_names.iter().any(|name| name == "update_node"));
        assert!(!read_names.iter().any(|name| name == "rename_node"));
        assert!(!read_names.iter().any(|name| name == "move_node"));
        assert!(!read_names.iter().any(|name| name == "reorder_node"));
        assert_eq!(write_names.len(), read_names.len() + 14);
        assert!(read_only
            .get_info()
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("Read-only")
                && text.contains("never emit a bare name")
                && text.contains("Never invent, concatenate, or infer")
                && text.contains("root_id as subtree's selector")));
        assert!(read_write
            .get_info()
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("read/write")
                && text.contains("initialize_workspace only when exposed")
                && text.contains("equivalent MCP tool is exposed")));
    }
}
