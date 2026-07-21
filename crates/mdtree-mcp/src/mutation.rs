//! Shared MCP mutation contracts and opt-in tool router.

use rmcp::handler::server::router::tool::ToolRouter;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::str::FromStr;

use mdtree_core::{
    generate_slug, Clock, CloneSubtreeRequest, NodeId, NodeMetadata, Reference, ReferenceOrigin,
    ReferenceTarget, ReferenceType, RenameSlugPolicy, Slug, SystemClock, SystemUlidGenerator,
    UlidGenerator,
};
use mdtree_sqlite::{
    prepare_node_mutation, AtomicTreeMove, AtomicTreeRemoval, NodeMutationDraft,
    PreparedBatchOperation, PreparedNodeMutation,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars;
use rmcp::{tool, tool_router, ErrorData};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::MdtreeServer;

/// Input for creating the configured workspace and its single root node.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct InitializeWorkspaceParams {
    /// Human-readable workspace and root title.
    pub name: String,
}

/// Input for exporting one node or a bounded subtree to Markdown files.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct ExportNodeParams {
    /// Existing node selected by ID, slug, or canonical path.
    pub selector: String,
    /// Existing output directory or new root Markdown file on the server filesystem.
    pub destination: String,
    /// Include descendants of the selected node.
    #[serde(default)]
    pub subtree: bool,
    /// Maximum relative depth. Omitted with `subtree=true` means all descendants.
    pub depth: Option<u32>,
}

const MAX_CREATE_NODE_COUNT: usize = 100;
const MAX_CREATE_NODE_DEPTH: usize = 20;

/// Input for one descendant created with its enclosing `create_node` request.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct CreateNodeChildParams {
    /// Human-readable title; also replaces `metadata.title` when metadata is supplied.
    pub title: String,
    /// Markdown body. Defaults to a level-one heading containing the title.
    pub content: Option<String>,
    /// Complete optional metadata; its title is normalized to `title`.
    pub metadata: Option<serde_json::Value>,
    /// Optional caller-selected ULID. A fresh ULID is generated when omitted.
    pub requested_id: Option<String>,
    /// Optional sibling order. Defaults to the position after the last child.
    pub sibling_order: Option<u32>,
    /// Descendants created below this node in the same atomic operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
}

/// Input for atomically creating one child node and optional descendants.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct CreateNodeParams {
    /// Stable caller-generated retry key.
    pub operation_id: Option<String>,
    /// Existing parent selected by ID, slug, or canonical path.
    pub parent: String,
    /// Human-readable title; also replaces `metadata.title` when metadata is supplied.
    pub title: String,
    /// Markdown body. Defaults to a level-one heading containing the title.
    pub content: Option<String>,
    /// Complete optional metadata; its title is normalized to `title`.
    pub metadata: Option<serde_json::Value>,
    /// Optional caller-selected ULID. A fresh ULID is generated when omitted.
    pub requested_id: Option<String>,
    /// Optional sibling order. Defaults to the position after the last child.
    pub sibling_order: Option<u32>,
    /// Descendants created below this node in the same atomic operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<CreateNodeChildParams>,
    /// Safety and immutable revision metadata.
    #[serde(default)]
    pub options: MutationOptions,
}

/// Input for atomically replacing mutable content or metadata.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct UpdateNodeParams {
    /// Stable caller-generated retry key.
    pub operation_id: Option<String>,
    /// Existing node selected by ID, slug, or canonical path.
    pub selector: String,
    /// Replacement Markdown body; omitted content remains unchanged.
    pub content: Option<String>,
    /// Replacement metadata; title is preserved because rename has a dedicated tool.
    pub metadata: Option<serde_json::Value>,
    /// Required optimistic concurrency observation.
    pub precondition: WritePrecondition,
    /// Safety and immutable revision metadata.
    #[serde(default)]
    pub options: MutationOptions,
}

/// Slug behavior for a rename.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpRenameSlugPolicy {
    /// Keep the canonical path stable.
    #[default]
    Preserve,
    /// Regenerate a unique slug from the new title.
    Regenerate,
}

/// Input for atomically renaming a node.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct RenameNodeParams {
    /// Stable caller-generated retry key.
    pub operation_id: Option<String>,
    /// Existing node selected by ID, slug, or canonical path.
    pub selector: String,
    /// Replacement human-readable title.
    pub title: String,
    /// Explicit canonical-path behavior.
    #[serde(default)]
    pub slug_policy: McpRenameSlugPolicy,
    /// Required optimistic concurrency observation.
    pub precondition: WritePrecondition,
    /// Safety and immutable revision metadata.
    #[serde(default)]
    pub options: MutationOptions,
}

/// Input for moving a subtree to a new parent.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct MoveNodeParams {
    /// Stable caller-generated retry key.
    pub operation_id: Option<String>,
    /// Existing subtree root selector.
    pub selector: String,
    /// Existing destination parent selector.
    pub destination_parent: String,
    /// Optional requested order; defaults after the destination's last child.
    pub sibling_order: Option<u32>,
    /// Required optimistic concurrency observation.
    pub precondition: WritePrecondition,
    /// Safety and immutable revision metadata.
    #[serde(default)]
    pub options: MutationOptions,
}

/// Input for atomically cloning one subtree with new identities.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct CloneSubtreeParams {
    /// Stable caller-generated retry key for the complete clone.
    pub operation_id: Option<String>,
    /// Existing subtree root selected by ID, slug, or canonical path.
    pub source: String,
    /// Existing destination parent selected by ID, slug, or canonical path.
    pub destination_parent: String,
    /// Optional zero-based position among the destination's children.
    pub sibling_order: Option<u32>,
    /// Required observation of the source node.
    pub precondition: WritePrecondition,
    /// Dry-run and immutable revision metadata.
    #[serde(default)]
    pub options: MutationOptions,
}

/// One subtree move within an atomic focused tree batch.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct AtomicTreeMoveParams {
    /// Existing subtree root selector.
    pub selector: String,
    /// Existing destination parent selector.
    pub destination_parent: String,
    /// Optional zero-based destination sibling position.
    pub sibling_order: Option<u32>,
    /// Required observation of the moved subtree root.
    pub precondition: WritePrecondition,
}

/// One guarded subtree removal within an atomic focused tree batch.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct AtomicTreeRemovalParams {
    /// Existing subtree root selector.
    pub selector: String,
    /// Required observed version of the subtree root.
    pub expected_version: u64,
    /// Explicit destructive confirmation; must be true.
    #[serde(default)]
    pub confirm: bool,
}

/// Input for applying unrelated moves and removals in one transaction.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct AtomicTreeBatchParams {
    /// Stable caller-generated retry key for the complete batch.
    pub operation_id: Option<String>,
    /// Unrelated subtree moves applied in request order.
    #[serde(default)]
    pub moves: Vec<AtomicTreeMoveParams>,
    /// Unrelated confirmed subtree removals applied in request order.
    #[serde(default)]
    pub removals: Vec<AtomicTreeRemovalParams>,
    /// Dry-run and immutable revision metadata shared by the batch.
    #[serde(default)]
    pub options: MutationOptions,
}

/// One operation in a heterogeneous atomic mutation batch.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MutationBatchOperationParams {
    Create {
        label: Option<String>,
        parent: String,
        title: String,
        content: Option<String>,
        requested_id: Option<String>,
        sibling_order: Option<u32>,
    },
    Update {
        selector: String,
        content: String,
        precondition: WritePrecondition,
    },
    Rename {
        selector: String,
        title: String,
        precondition: WritePrecondition,
    },
    Move {
        selector: String,
        destination_parent: String,
        sibling_order: Option<u32>,
        precondition: WritePrecondition,
    },
    Reorder {
        selector: String,
        sibling_order: u32,
        precondition: WritePrecondition,
    },
    Remove {
        selector: String,
        expected_version: u64,
        #[serde(default)]
        confirm: bool,
    },
    SetReferences {
        selector: String,
        references: Vec<ExplicitReferenceInput>,
        precondition: WritePrecondition,
    },
}

/// Input for up to 50 heterogeneous mutations committed together.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct MutationBatchParams {
    /// Stable caller-generated retry key for the complete batch.
    pub operation_id: Option<String>,
    /// One through 50 ordered, concretely typed operations.
    pub operations: Vec<MutationBatchOperationParams>,
    /// Dry-run and immutable revision metadata shared by the batch.
    #[serde(default)]
    pub options: MutationOptions,
}

/// Input for changing a node's position among its current siblings.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct ReorderNodeParams {
    /// Stable caller-generated retry key.
    pub operation_id: Option<String>,
    /// Existing node selector.
    pub selector: String,
    /// Requested zero-based sibling position.
    pub sibling_order: u32,
    /// Required optimistic concurrency observation.
    pub precondition: WritePrecondition,
    /// Safety and immutable revision metadata.
    #[serde(default)]
    pub options: MutationOptions,
}

/// Input for guarded subtree removal.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct RemoveNodeParams {
    pub operation_id: Option<String>,
    pub selector: String,
    pub expected_version: u64,
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub options: MutationOptions,
}

/// How supplied explicit references modify the existing set.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceMutationMode {
    Add,
    Set,
    Remove,
}

/// One requested explicit typed reference.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct ExplicitReferenceInput {
    pub reference_type: String,
    pub target: String,
    pub anchor: Option<String>,
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Input for add/set/remove explicit-reference mutations.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct SetReferencesParams {
    pub operation_id: Option<String>,
    pub selector: String,
    pub mode: ReferenceMutationMode,
    pub references: Vec<ExplicitReferenceInput>,
    pub precondition: WritePrecondition,
    #[serde(default)]
    pub options: MutationOptions,
}

/// Input for restoring an immutable revision as a new head.
#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
pub struct RestoreVersionParams {
    pub operation_id: Option<String>,
    pub selector: String,
    pub target_version: u64,
    pub precondition: WritePrecondition,
    #[serde(default)]
    pub options: MutationOptions,
}

/// Server access boundary selected at startup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum McpAccessMode {
    /// Only tools that do not modify the workspace or filesystem are exposed.
    #[default]
    ReadOnly,
    /// Workspace mutation and filesystem export tools are registered in addition to read tools.
    ReadWrite,
}

impl McpAccessMode {
    /// Whether write-tool registration is permitted.
    #[must_use]
    pub const fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// Optimistic concurrency precondition shared by versioned mutations.
#[derive(
    Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema,
)]
pub struct WritePrecondition {
    /// Caller-observed version; at least one precondition must be supplied.
    pub expected_version: Option<u64>,
    /// Caller-observed lowercase hexadecimal BLAKE3 content hash.
    pub expected_content_hash: Option<String>,
}

impl WritePrecondition {
    /// Validates that at least one usable optimistic precondition is present.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.expected_version.is_none() && self.expected_content_hash.is_none() {
            return Err("expected_version or expected_content_hash is required");
        }
        if self.expected_version == Some(0) {
            return Err("expected_version must be at least 1");
        }
        if self.expected_content_hash.as_deref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err("expected_content_hash must be 64 lowercase hexadecimal characters");
        }
        Ok(())
    }
}

/// Common safety and revision metadata for MCP mutations.
#[derive(
    Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema,
)]
pub struct MutationOptions {
    /// Validate and return a plan without changing canonical state.
    #[serde(default)]
    pub dry_run: bool,
    /// Human or agent identity recorded on the immutable revision.
    pub author: Option<String>,
    /// Concise reason recorded on the immutable revision.
    pub change_summary: Option<String>,
    /// Run bounded integrity validation after a successful write.
    #[serde(default)]
    pub validate_after_write: bool,
}

/// Stable mutation outcome category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    /// Dry-run validation succeeded without mutation.
    Planned,
    /// Canonical state and derived records changed atomically.
    Applied,
    /// Requested semantic state already matched the head.
    NoOp,
}

/// Bounded canonical node state returned by mutation tools.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
pub struct MutationNodeResult {
    /// Stable identity.
    pub id: String,
    /// Parent identity.
    pub parent_id: String,
    /// Canonical sibling-unique path segment.
    pub slug: String,
    /// Canonical descriptive metadata.
    pub metadata: serde_json::Value,
    /// Canonical Markdown body.
    pub content: String,
    /// Zero-based sibling order.
    pub sibling_order: u32,
}

/// Bounded response shared by all MCP mutation tools.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
pub struct MutationResult {
    /// Outcome category.
    pub status: MutationStatus,
    /// Stable affected node identity.
    pub node_id: String,
    /// Resulting or proposed version.
    pub version: u64,
    /// Lowercase hexadecimal content hash.
    pub content_hash: String,
    /// Lowercase hexadecimal semantic revision hash.
    pub revision_hash: String,
    /// Resulting or proposed canonical path.
    pub path: String,
    /// Resulting or proposed canonical node state.
    pub node: MutationNodeResult,
    /// Non-fatal planning or convention findings.
    pub warnings: Vec<String>,
}

/// Creation result retaining the root-node response and listing all descendants.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
pub struct CreateNodeResult {
    /// Existing mutation response fields for the requested root node.
    #[serde(flatten)]
    pub mutation: MutationResult,
    /// Created descendants in deterministic parent-first order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendants: Vec<MutationResult>,
}

/// Move result with bounded subtree path impact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
pub struct MoveMutationResult {
    /// Common mutation result and proposed destination path.
    #[serde(flatten)]
    pub mutation: MutationResult,
    /// Canonical path before the move.
    pub previous_path: String,
    /// Number of nodes whose canonical paths move with the subtree.
    pub affected_node_count: usize,
}

/// Guarded subtree-removal impact and outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
pub struct RemoveMutationResult {
    pub status: MutationStatus,
    pub node_id: String,
    pub affected_node_ids: Vec<String>,
    pub external_backlinks: Vec<serde_json::Value>,
    pub warnings: Vec<String>,
}

/// Stable structured detail attached to MCP mutation errors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, rmcp::schemars::JsonSchema)]
pub struct MutationErrorDetail {
    /// Stable machine-readable category.
    pub code: String,
    /// Conflicting node when applicable.
    pub node_id: Option<String>,
    /// Expected version when applicable.
    pub expected_version: Option<u64>,
    /// Persisted version when applicable.
    pub actual_version: Option<u64>,
}

impl From<&mdtree_sqlite::StoreError> for MutationErrorDetail {
    fn from(error: &mdtree_sqlite::StoreError) -> Self {
        let (code, node_id, expected_version, actual_version) = match error {
            mdtree_sqlite::StoreError::NotFound(_) => ("not_found", None, None, None),
            mdtree_sqlite::StoreError::Ambiguous(_) => ("ambiguous", None, None, None),
            mdtree_sqlite::StoreError::Conflict {
                node_id,
                expected,
                actual,
            } => (
                "conflict",
                Some(node_id.to_string()),
                Some(*expected),
                Some(*actual),
            ),
            mdtree_sqlite::StoreError::IdempotencyConflict(_) => {
                ("idempotency_conflict", None, None, None)
            }
            mdtree_sqlite::StoreError::Invariant(_) => ("invariant", None, None, None),
            mdtree_sqlite::StoreError::InvalidData(_) => ("invalid_data", None, None, None),
            mdtree_sqlite::StoreError::BudgetExceeded { .. } => {
                ("budget_exceeded", None, None, None)
            }
            mdtree_sqlite::StoreError::Sqlite(_) => ("storage", None, None, None),
            mdtree_sqlite::StoreError::Json(_) => ("serialization", None, None, None),
        };
        Self {
            code: code.into(),
            node_id,
            expected_version,
            actual_version,
        }
    }
}

#[tool_router(router = mutation_tool_router)]
impl MdtreeServer {
    pub(crate) fn write_tool_router() -> ToolRouter<Self> {
        Self::mutation_tool_router()
    }

    /// Advertises the opt-in mutation contract before concrete mutation tools land.
    #[tool(description = "Describe enabled MDTree mutation safety and concurrency contracts")]
    async fn mutation_capabilities(&self) -> Result<CallToolResult, ErrorData> {
        let value = serde_json::json!({
            "access_mode": "read_write",
            "optimistic_concurrency": ["expected_version", "expected_content_hash"],
            "dry_run": true,
            "revision_metadata": ["author", "change_summary"],
            "mutation_tools": [
                "initialize_workspace", "create_node", "update_node", "rename_node", "move_node", "clone_subtree", "atomic_tree_batch", "mutation_batch", "reorder_node",
                "remove_node", "set_references", "restore_version"
            ],
            "filesystem_tools": ["export_node"],
            "idempotency": "persisted_operation_receipts",
            "create_node_descendants": {"atomic": true, "maximum_nodes": MAX_CREATE_NODE_COUNT, "maximum_depth": MAX_CREATE_NODE_DEPTH},
            "post_write_validation": true
        });
        Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
    }

    /// Creates the configured workspace with exactly one root node.
    #[tool(description = "Initialize the configured MDTree workspace with one root node")]
    async fn initialize_workspace(
        &self,
        Parameters(params): Parameters<InitializeWorkspaceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = params.name.trim();
        if name.is_empty() {
            return Err(crate::invalid("name must not be blank"));
        }
        let mut binding = self
            .binding
            .lock()
            .map_err(|_| crate::mcp_error("workspace lock poisoned"))?;
        if binding.store.is_some() || binding.path.exists() {
            return Err(crate::invalid("workspace is already initialized"));
        }

        let id = NodeId::new(SystemUlidGenerator.generate());
        let now = SystemClock.now_millis();
        let root = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: None,
                slug: mdtree_core::generate_slug(name, std::iter::empty()),
                metadata: NodeMetadata::new(name),
                markdown_content: format!("# {name}\n"),
                sibling_order: 0,
                version: 1,
                created_at: now,
                updated_at: now,
                created_by: Some("mcp".into()),
                change_summary: Some("Initialize workspace via MCP".into()),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?
        .node;
        let connection = mdtree_sqlite::create_workspace(&binding.path, name, &root)
            .map_err(|error| crate::mcp_error(error.to_string()))?;
        drop(connection);
        binding.store = Some(
            mdtree_sqlite::SqliteStore::open(&binding.path)
                .map_err(|error| crate::mcp_error(error.to_string()))?,
        );
        crate::json_result(serde_json::json!({
            "status": "initialized",
            "workspace": binding.path,
            "name": name,
            "root_id": id,
        }))
    }

    /// Exports one node or a bounded subtree without modifying the workspace.
    #[tool(description = "Export an MDTree node or subtree to Markdown files on the server")]
    async fn export_node(
        &self,
        Parameters(params): Parameters<ExportNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.destination.trim().is_empty() {
            return Err(crate::invalid("destination must not be blank"));
        }
        if params.depth.is_some() && !params.subtree {
            return Err(crate::invalid("depth requires subtree=true"));
        }
        let (store, id) = self.store_and_id(&params.selector)?;
        let node = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let max_depth = if params.subtree {
            params.depth
        } else {
            Some(0)
        };
        let destination = PathBuf::from(&params.destination);
        let root_file = if destination.is_dir() {
            destination.join(format!("{}.md", node.fields().slug))
        } else {
            destination.clone()
        };
        let exported_files =
            mdtree_sqlite::export_markdown_node(&store, id, &destination, max_depth)
                .map_err(|error| crate::mcp_error(error.to_string()))?;
        crate::json_result(serde_json::json!({
            "status": "exported",
            "node_id": id,
            "destination": destination,
            "root_file": root_file,
            "subtree": params.subtree,
            "depth": max_depth,
            "exported_node_count": exported_files.len(),
        }))
    }

    /// Creates one child node and optional descendants atomically.
    #[tool(
        description = "Create an MDTree node and optional descendants atomically, with optional dry-run"
    )]
    async fn create_node(
        &self,
        Parameters(params): Parameters<CreateNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<CreateNodeResult>(
            &store,
            params.operation_id.as_ref(),
            "create_node",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        let parent_id = Self::id_in(&store, &params.parent)?;
        let now = SystemClock.now_millis();
        let parent_path = store
            .canonical_path(parent_id)
            .map_err(crate::store_error)?;
        let existing_children = store.children(parent_id).map_err(crate::store_error)?;
        let mut sibling_slugs = existing_children
            .iter()
            .map(|node| node.fields().slug.clone())
            .collect::<Vec<_>>();
        let mut next_order = existing_children
            .iter()
            .map(|node| node.fields().sibling_order)
            .max()
            .map_or(0, |order| order.saturating_add(1));
        let mut prepared = Vec::new();
        let mut paths = Vec::new();
        let mut ids = BTreeSet::new();
        prepare_create_branch(
            &store,
            parent_id,
            &parent_path,
            &params.title,
            params.content.as_deref(),
            params.metadata.as_ref(),
            params.requested_id.as_deref(),
            params.sibling_order,
            &params.children,
            &params.options,
            now,
            0,
            &mut sibling_slugs,
            &mut next_order,
            &mut ids,
            &mut prepared,
            &mut paths,
        )?;
        let status = if params.options.dry_run {
            MutationStatus::Planned
        } else {
            store.create_nodes(&prepared).map_err(crate::store_error)?;
            MutationStatus::Applied
        };
        let mut node_results = prepared
            .iter()
            .zip(paths)
            .map(|(node, path)| mutation_result(node, status, 1, path, Vec::new()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut root = node_results.remove(0);
        root.warnings = post_validation(
            &store,
            params.options.validate_after_write && status == MutationStatus::Applied,
        )?;
        let result = CreateNodeResult {
            mutation: root,
            descendants: node_results,
        };
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "create_node",
            &payload_hash,
            &result,
            now,
        )?;
        crate::json_result(result)
    }

    /// Clones a complete subtree atomically with explicit-reference remapping.
    #[tool(
        description = "Clone an MDTree subtree atomically with new identities and remapped internal references"
    )]
    async fn clone_subtree(
        &self,
        Parameters(params): Parameters<CloneSubtreeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<serde_json::Value>(
            &store,
            params.operation_id.as_ref(),
            "clone_subtree",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let source_id = Self::id_in(&store, &params.source)?;
        let destination_parent_id = Self::id_in(&store, &params.destination_parent)?;
        let source = store
            .get(source_id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("source node not found"))?;
        validate_precondition(
            source_id,
            source.fields().version,
            source.fields().content_hash.as_bytes(),
            &params.precondition,
        )?;
        let now = SystemClock.now_millis();
        let result = store
            .clone_subtree(
                &CloneSubtreeRequest {
                    source_id,
                    destination_parent_id,
                    expected_version: source.fields().version,
                    sibling_order: params.sibling_order,
                    dry_run: params.options.dry_run,
                    created_at: now,
                    created_by: params.options.author.clone(),
                    change_summary: params.options.change_summary.clone(),
                },
                &SystemUlidGenerator,
            )
            .map_err(crate::store_error)?;
        let mut value = serde_json::to_value(result).map_err(crate::json_error)?;
        value["warnings"] = serde_json::to_value(post_validation(
            &store,
            params.options.validate_after_write && !params.options.dry_run,
        )?)
        .map_err(crate::json_error)?;
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "clone_subtree",
            &payload_hash,
            &value,
            now,
        )?;
        crate::json_result(value)
    }

    /// Applies unrelated subtree moves and guarded removals in one transaction.
    #[tool(
        description = "Validate and atomically apply up to 50 unrelated MDTree moves and removals"
    )]
    #[allow(clippy::too_many_lines)]
    async fn atomic_tree_batch(
        &self,
        Parameters(params): Parameters<AtomicTreeBatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<serde_json::Value>(
            &store,
            params.operation_id.as_ref(),
            "atomic_tree_batch",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        if params.removals.iter().any(|item| !item.confirm) {
            return Err(crate::invalid(
                "every atomic tree batch removal requires confirm=true",
            ));
        }
        let now = SystemClock.now_millis();
        let mut moves = Vec::with_capacity(params.moves.len());
        for item in &params.moves {
            item.precondition.validate().map_err(crate::invalid)?;
            let id = Self::id_in(&store, &item.selector)?;
            let destination_id = Self::id_in(&store, &item.destination_parent)?;
            let current = store
                .get(id)
                .map_err(crate::store_error)?
                .ok_or_else(|| crate::invalid("node not found"))?;
            let fields = current.fields();
            validate_precondition(
                id,
                fields.version,
                fields.content_hash.as_bytes(),
                &item.precondition,
            )?;
            let destination_children = store
                .children(destination_id)
                .map_err(crate::store_error)?
                .into_iter()
                .filter(|node| node.id() != id)
                .collect::<Vec<_>>();
            let slug = generate_slug(
                &fields.metadata.title,
                destination_children.iter().map(|node| &node.fields().slug),
            );
            let default_order = destination_children
                .iter()
                .map(|node| node.fields().sibling_order)
                .max()
                .map_or(0, |order| order.saturating_add(1));
            let prepared = prepare_node_mutation(
                NodeMutationDraft {
                    id,
                    parent_id: Some(destination_id),
                    slug,
                    metadata: fields.metadata.clone(),
                    markdown_content: fields.markdown_content.clone(),
                    sibling_order: item.sibling_order.unwrap_or(default_order),
                    version: fields.version + 1,
                    created_at: fields.created_at,
                    updated_at: now,
                    created_by: params.options.author.clone(),
                    change_summary: params
                        .options
                        .change_summary
                        .clone()
                        .or_else(|| Some("Atomic tree batch move via MCP".into())),
                },
                &SystemUlidGenerator,
            )
            .map_err(crate::store_error)?;
            moves.push(AtomicTreeMove {
                prepared,
                expected_version: fields.version,
            });
        }
        let mut removals = Vec::with_capacity(params.removals.len());
        for item in &params.removals {
            if item.expected_version == 0 {
                return Err(crate::invalid("expected_version must be at least 1"));
            }
            removals.push(AtomicTreeRemoval {
                node_id: Self::id_in(&store, &item.selector)?,
                expected_version: item.expected_version,
            });
        }
        let result = store
            .apply_atomic_tree_batch(&moves, &removals, params.options.dry_run)
            .map_err(crate::store_error)?;
        let mut value = serde_json::to_value(result).map_err(crate::json_error)?;
        value["warnings"] = serde_json::to_value(post_validation(
            &store,
            params.options.validate_after_write && !params.options.dry_run,
        )?)
        .map_err(crate::json_error)?;
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "atomic_tree_batch",
            &payload_hash,
            &value,
            now,
        )?;
        crate::json_result(value)
    }

    /// Applies heterogeneous create/update/rename/move/reorder/remove/reference operations.
    #[tool(
        description = "Validate and atomically apply up to 50 heterogeneous MDTree mutations with temporary create labels"
    )]
    async fn mutation_batch(
        &self,
        Parameters(params): Parameters<MutationBatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<serde_json::Value>(
            &store,
            params.operation_id.as_ref(),
            "mutation_batch",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        let now = SystemClock.now_millis();
        let operations = prepare_mcp_batch(&store, &params, now)?;
        let result = store
            .apply_mutation_batch(&operations, params.options.dry_run)
            .map_err(crate::store_error)?;
        let mut value = serde_json::to_value(result).map_err(crate::json_error)?;
        value["warnings"] = serde_json::to_value(post_validation(
            &store,
            params.options.validate_after_write && !params.options.dry_run,
        )?)
        .map_err(crate::json_error)?;
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "mutation_batch",
            &payload_hash,
            &value,
            now,
        )?;
        crate::json_result(value)
    }

    /// Replaces node content or metadata with optimistic concurrency protection.
    #[tool(
        description = "Update MDTree node content or metadata atomically, with required precondition"
    )]
    async fn update_node(
        &self,
        Parameters(params): Parameters<UpdateNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<MutationResult>(
            &store,
            params.operation_id.as_ref(),
            "update_node",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let id = Self::id_in(&store, &params.selector)?;
        let current = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let current_fields = current.fields();
        let actual_hash = hash_hex(current_fields.content_hash.as_bytes());
        let version_matches = params
            .precondition
            .expected_version
            .is_none_or(|expected| expected == current_fields.version);
        let hash_matches = params
            .precondition
            .expected_content_hash
            .as_deref()
            .is_none_or(|expected| expected == actual_hash);
        if !version_matches || !hash_matches {
            return Err(precondition_conflict(
                id,
                params.precondition.expected_version,
                current_fields.version,
            ));
        }
        let expected_version = current_fields.version;
        let mut metadata = params
            .metadata
            .map(serde_json::from_value::<NodeMetadata>)
            .transpose()
            .map_err(|error| crate::invalid(format!("invalid metadata: {error}")))?
            .unwrap_or_else(|| current_fields.metadata.clone());
        metadata.title.clone_from(&current_fields.metadata.title);
        let prepared = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: current.parent_id(),
                slug: current_fields.slug.clone(),
                metadata,
                markdown_content: params
                    .content
                    .unwrap_or_else(|| current_fields.markdown_content.clone()),
                sibling_order: current_fields.sibling_order,
                version: expected_version + 1,
                created_at: current_fields.created_at,
                updated_at: SystemClock.now_millis(),
                created_by: params.options.author,
                change_summary: params
                    .options
                    .change_summary
                    .or_else(|| Some("Update node via MCP".into())),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?;
        let semantic_no_op = prepared.node.fields().revision_hash == current_fields.revision_hash;
        let status = if semantic_no_op {
            MutationStatus::NoOp
        } else if params.options.dry_run {
            MutationStatus::Planned
        } else {
            match store
                .update_node(prepared.change(expected_version))
                .map_err(crate::store_error)?
            {
                mdtree_sqlite::MutationOutcome::Applied => MutationStatus::Applied,
                mdtree_sqlite::MutationOutcome::NoOp => MutationStatus::NoOp,
            }
        };
        let path = store
            .canonical_path(id)
            .map_err(crate::store_error)?
            .into_iter()
            .map(|slug| slug.to_string())
            .collect::<Vec<_>>()
            .join("/");
        let fields = prepared.node.fields();
        let result = MutationResult {
            status,
            node_id: id.to_string(),
            version: if semantic_no_op {
                expected_version
            } else {
                fields.version
            },
            content_hash: hash_hex(fields.content_hash.as_bytes()),
            revision_hash: hash_hex(fields.revision_hash.as_bytes()),
            path,
            node: MutationNodeResult {
                id: id.to_string(),
                parent_id: current
                    .parent_id()
                    .map_or_else(String::new, |parent| parent.to_string()),
                slug: fields.slug.to_string(),
                metadata: serde_json::to_value(&fields.metadata).map_err(crate::json_error)?,
                content: fields.markdown_content.clone(),
                sibling_order: fields.sibling_order,
            },
            warnings: post_validation(
                &store,
                params.options.validate_after_write && status == MutationStatus::Applied,
            )?,
        };
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "update_node",
            &payload_hash,
            &result,
            SystemClock.now_millis(),
        )?;
        crate::json_result(result)
    }

    /// Renames a node while explicitly controlling canonical slug behavior.
    #[tool(description = "Rename an MDTree node atomically with explicit slug policy")]
    async fn rename_node(
        &self,
        Parameters(params): Parameters<RenameNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<MutationResult>(
            &store,
            params.operation_id.as_ref(),
            "rename_node",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let id = Self::id_in(&store, &params.selector)?;
        let current = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let fields = current.fields();
        validate_precondition(
            id,
            fields.version,
            fields.content_hash.as_bytes(),
            &params.precondition,
        )?;
        let sibling_slugs = if let Some(parent) = current.parent_id() {
            store
                .children(parent)
                .map_err(crate::store_error)?
                .into_iter()
                .filter(|node| node.id() != id)
                .map(|node| node.fields().slug.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let policy = match params.slug_policy {
            McpRenameSlugPolicy::Preserve => RenameSlugPolicy::Preserve,
            McpRenameSlugPolicy::Regenerate => RenameSlugPolicy::Regenerate,
        };
        let slug =
            mdtree_core::slug_for_rename(&fields.slug, &params.title, &sibling_slugs, policy);
        let base_slug = mdtree_core::generate_slug(&params.title, std::iter::empty());
        let warnings =
            if matches!(params.slug_policy, McpRenameSlugPolicy::Regenerate) && slug != base_slug {
                vec![format!("slug collision resolved as {slug}")]
            } else {
                Vec::new()
            };
        let mut metadata = fields.metadata.clone();
        metadata.title.clone_from(&params.title);
        let now = SystemClock.now_millis();
        let prepared = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: current.parent_id(),
                slug,
                metadata,
                markdown_content: fields.markdown_content.clone(),
                sibling_order: fields.sibling_order,
                version: fields.version + 1,
                created_at: fields.created_at,
                updated_at: now,
                created_by: params.options.author,
                change_summary: params
                    .options
                    .change_summary
                    .or_else(|| Some("Rename node via MCP".into())),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?;
        let semantic_no_op = prepared.node.fields().revision_hash == fields.revision_hash;
        let status = if semantic_no_op {
            MutationStatus::NoOp
        } else if params.options.dry_run {
            MutationStatus::Planned
        } else {
            match store
                .rename_node(prepared.change(fields.version))
                .map_err(crate::store_error)?
            {
                mdtree_sqlite::MutationOutcome::Applied => MutationStatus::Applied,
                mdtree_sqlite::MutationOutcome::NoOp => MutationStatus::NoOp,
            }
        };
        let proposed = prepared.node.fields();
        let mut path = if let Some(parent) = current.parent_id() {
            store.canonical_path(parent).map_err(crate::store_error)?
        } else {
            Vec::new()
        };
        path.push(proposed.slug.clone());
        let mut warnings = warnings;
        warnings.extend(post_validation(
            &store,
            params.options.validate_after_write && status == MutationStatus::Applied,
        )?);
        let result = MutationResult {
            status,
            node_id: id.to_string(),
            version: if semantic_no_op {
                fields.version
            } else {
                proposed.version
            },
            content_hash: hash_hex(proposed.content_hash.as_bytes()),
            revision_hash: hash_hex(proposed.revision_hash.as_bytes()),
            path: path
                .into_iter()
                .map(|slug| slug.to_string())
                .collect::<Vec<_>>()
                .join("/"),
            node: MutationNodeResult {
                id: id.to_string(),
                parent_id: current
                    .parent_id()
                    .map_or_else(String::new, |parent| parent.to_string()),
                slug: proposed.slug.to_string(),
                metadata: serde_json::to_value(&proposed.metadata).map_err(crate::json_error)?,
                content: proposed.markdown_content.clone(),
                sibling_order: proposed.sibling_order,
            },
            warnings,
        };
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "rename_node",
            &payload_hash,
            &result,
            now,
        )?;
        crate::json_result(result)
    }

    /// Moves a subtree to a new parent with deterministic placement.
    #[tool(description = "Move an MDTree subtree atomically with dry-run path impact")]
    #[allow(clippy::too_many_lines)]
    async fn move_node(
        &self,
        Parameters(params): Parameters<MoveNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<MoveMutationResult>(
            &store,
            params.operation_id.as_ref(),
            "move_node",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let id = Self::id_in(&store, &params.selector)?;
        let destination_id = Self::id_in(&store, &params.destination_parent)?;
        let current = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let fields = current.fields();
        validate_precondition(
            id,
            fields.version,
            fields.content_hash.as_bytes(),
            &params.precondition,
        )?;
        if current.is_root() {
            return Err(crate::invalid("workspace root cannot be moved"));
        }
        let descendants = store.descendants(id).map_err(crate::store_error)?;
        if destination_id == id
            || descendants
                .iter()
                .any(|item| item.node.id() == destination_id)
        {
            return Err(crate::invalid("cannot move a node below its own subtree"));
        }
        let destination = store
            .get(destination_id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("destination parent not found"))?;
        if !destination.fields().metadata.accepts_children.is_empty()
            && fields.metadata.node_type.as_ref().is_none_or(|node_type| {
                !destination
                    .fields()
                    .metadata
                    .accepts_children
                    .contains(node_type)
            })
        {
            return Err(crate::invalid(
                "destination parent does not accept the node type",
            ));
        }
        let destination_children = store
            .children(destination_id)
            .map_err(crate::store_error)?
            .into_iter()
            .filter(|node| node.id() != id)
            .collect::<Vec<_>>();
        let same_parent = current.parent_id() == Some(destination_id);
        let slug = mdtree_core::generate_slug(
            &fields.metadata.title,
            destination_children.iter().map(|node| &node.fields().slug),
        );
        let last_insertion_index = u32::try_from(destination_children.len()).unwrap_or(u32::MAX);
        let order = params
            .sibling_order
            .unwrap_or(last_insertion_index)
            .min(last_insertion_index);
        let previous_path = store
            .canonical_path(id)
            .map_err(crate::store_error)?
            .into_iter()
            .map(|slug| slug.to_string())
            .collect::<Vec<_>>()
            .join("/");
        let now = SystemClock.now_millis();
        let prepared = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: Some(destination_id),
                slug,
                metadata: fields.metadata.clone(),
                markdown_content: fields.markdown_content.clone(),
                sibling_order: order,
                version: fields.version + 1,
                created_at: fields.created_at,
                updated_at: now,
                created_by: params.options.author,
                change_summary: params
                    .options
                    .change_summary
                    .or_else(|| Some("Move subtree via MCP".into())),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?;
        let semantic_no_op = prepared.node.fields().revision_hash == fields.revision_hash;
        let status = if semantic_no_op {
            MutationStatus::NoOp
        } else if params.options.dry_run {
            MutationStatus::Planned
        } else {
            let outcome = if same_parent {
                store.reorder_node(prepared.change(fields.version))
            } else {
                store.move_subtree(prepared.change(fields.version))
            }
            .map_err(crate::store_error)?;
            match outcome {
                mdtree_sqlite::MutationOutcome::Applied => MutationStatus::Applied,
                mdtree_sqlite::MutationOutcome::NoOp => MutationStatus::NoOp,
            }
        };
        let proposed = prepared.node.fields();
        let mut path = store
            .canonical_path(destination_id)
            .map_err(crate::store_error)?;
        path.push(proposed.slug.clone());
        let result = MoveMutationResult {
            mutation: mutation_result(
                &prepared,
                status,
                if semantic_no_op {
                    fields.version
                } else {
                    proposed.version
                },
                path,
                post_validation(
                    &store,
                    params.options.validate_after_write && status == MutationStatus::Applied,
                )?,
            )?,
            previous_path,
            affected_node_count: descendants.len() + 1,
        };
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "move_node",
            &payload_hash,
            &result,
            now,
        )?;
        crate::json_result(result)
    }

    /// Reorders a node among its current siblings and normalizes all positions.
    #[tool(description = "Reorder an MDTree node deterministically among its siblings")]
    async fn reorder_node(
        &self,
        Parameters(params): Parameters<ReorderNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<MutationResult>(
            &store,
            params.operation_id.as_ref(),
            "reorder_node",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let id = Self::id_in(&store, &params.selector)?;
        let current = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let Some(parent_id) = current.parent_id() else {
            return Err(crate::invalid("workspace root cannot be reordered"));
        };
        let fields = current.fields();
        validate_precondition(
            id,
            fields.version,
            fields.content_hash.as_bytes(),
            &params.precondition,
        )?;
        let sibling_count = store.children(parent_id).map_err(crate::store_error)?.len();
        let order = params
            .sibling_order
            .min(u32::try_from(sibling_count.saturating_sub(1)).unwrap_or(u32::MAX));
        let now = SystemClock.now_millis();
        let prepared = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: Some(parent_id),
                slug: fields.slug.clone(),
                metadata: fields.metadata.clone(),
                markdown_content: fields.markdown_content.clone(),
                sibling_order: order,
                version: fields.version + 1,
                created_at: fields.created_at,
                updated_at: now,
                created_by: params.options.author,
                change_summary: params
                    .options
                    .change_summary
                    .or_else(|| Some("Reorder node via MCP".into())),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?;
        let semantic_no_op = prepared.node.fields().revision_hash == fields.revision_hash;
        let status = if semantic_no_op {
            MutationStatus::NoOp
        } else if params.options.dry_run {
            MutationStatus::Planned
        } else {
            match store
                .reorder_node(prepared.change(fields.version))
                .map_err(crate::store_error)?
            {
                mdtree_sqlite::MutationOutcome::Applied => MutationStatus::Applied,
                mdtree_sqlite::MutationOutcome::NoOp => MutationStatus::NoOp,
            }
        };
        let result = mutation_result(
            &prepared,
            status,
            if semantic_no_op {
                fields.version
            } else {
                fields.version + 1
            },
            store.canonical_path(id).map_err(crate::store_error)?,
            post_validation(
                &store,
                params.options.validate_after_write && status == MutationStatus::Applied,
            )?,
        )?;
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "reorder_node",
            &payload_hash,
            &result,
            now,
        )?;
        crate::json_result(result)
    }

    /// Removes a non-root subtree after impact analysis and confirmation.
    #[tool(description = "Analyze or remove an MDTree subtree with explicit confirmation")]
    async fn remove_node(
        &self,
        Parameters(params): Parameters<RemoveNodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<RemoveMutationResult>(
            &store,
            params.operation_id.as_ref(),
            "remove_node",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        if params.expected_version == 0 {
            return Err(crate::invalid("expected_version must be at least 1"));
        }
        if !params.options.dry_run && !params.confirm {
            return Err(crate::invalid(
                "confirm=true is required to remove a subtree",
            ));
        }
        let id = Self::id_in(&store, &params.selector)?;
        let subtree = store.subtree(id).map_err(crate::store_error)?;
        let affected = subtree
            .iter()
            .map(|item| item.node.id())
            .collect::<Vec<_>>();
        let affected_set = affected
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut backlinks = Vec::new();
        for target in &affected {
            for reference in store.backlinks(*target).map_err(crate::store_error)? {
                if !affected_set.contains(&reference.source_node_id) {
                    backlinks.push(serde_json::to_value(reference).map_err(crate::json_error)?);
                }
            }
        }
        store
            .remove_subtree(id, params.expected_version, params.options.dry_run)
            .map_err(crate::store_error)?;
        let mut warnings = post_validation(
            &store,
            params.options.validate_after_write && !params.options.dry_run,
        )?;
        if !backlinks.is_empty() {
            warnings.push("external backlinks become unresolved".into());
        }
        let result = RemoveMutationResult {
            status: if params.options.dry_run {
                MutationStatus::Planned
            } else {
                MutationStatus::Applied
            },
            node_id: id.to_string(),
            affected_node_ids: affected.into_iter().map(|node| node.to_string()).collect(),
            external_backlinks: backlinks,
            warnings,
        };
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "remove_node",
            &payload_hash,
            &result,
            SystemClock.now_millis(),
        )?;
        crate::json_result(result)
    }

    /// Adds, replaces, or removes explicit typed references atomically.
    #[tool(description = "Add, set, or remove explicit typed MDTree references")]
    #[allow(clippy::too_many_lines)]
    async fn set_references(
        &self,
        Parameters(params): Parameters<SetReferencesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<MutationResult>(
            &store,
            params.operation_id.as_ref(),
            "set_references",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let id = Self::id_in(&store, &params.selector)?;
        let current = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let fields = current.fields();
        validate_precondition(
            id,
            fields.version,
            fields.content_hash.as_bytes(),
            &params.precondition,
        )?;
        let mut requested = Vec::new();
        for input in &params.references {
            let reference_type = ReferenceType::from_str(&input.reference_type)
                .map_err(|error| crate::invalid(error.to_string()))?;
            let target = match store
                .resolve_reference(&input.target)
                .map_err(crate::store_error)?
            {
                mdtree_sqlite::ReferenceResolution::Resolved { node_id, .. } => {
                    ReferenceTarget::Resolved {
                        node_id,
                        target_ref: Some(input.target.clone()),
                        anchor: input.anchor.clone(),
                    }
                }
                _ => ReferenceTarget::Unresolved {
                    target_ref: input.target.clone(),
                },
            };
            requested.push(Reference {
                source_node_id: id,
                source_section_id: None,
                reference_type,
                target,
                origin: ReferenceOrigin::Explicit,
                metadata: input.metadata.clone(),
            });
        }
        let mut references = store
            .outgoing_references(id)
            .map_err(crate::store_error)?
            .into_iter()
            .filter(|reference| reference.origin == ReferenceOrigin::Explicit)
            .collect::<Vec<_>>();
        match params.mode {
            ReferenceMutationMode::Set => references = requested,
            ReferenceMutationMode::Add => references.extend(requested),
            ReferenceMutationMode::Remove => {
                references.retain(|existing| !requested.contains(existing));
            }
        }
        let mut unique = std::collections::BTreeSet::new();
        for reference in &references {
            let key = format!(
                "{}:{}",
                reference.reference_type,
                serde_json::to_string(&reference.target).map_err(crate::json_error)?
            );
            if !unique.insert(key) {
                return Err(crate::invalid("duplicate explicit reference"));
            }
        }
        let mut metadata = fields.metadata.clone();
        metadata.extensions.insert(
            "explicit_relations".into(),
            serde_json::to_value(&references).map_err(crate::json_error)?,
        );
        let now = SystemClock.now_millis();
        let prepared = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: current.parent_id(),
                slug: fields.slug.clone(),
                metadata,
                markdown_content: fields.markdown_content.clone(),
                sibling_order: fields.sibling_order,
                version: fields.version + 1,
                created_at: fields.created_at,
                updated_at: now,
                created_by: params.options.author,
                change_summary: params
                    .options
                    .change_summary
                    .or_else(|| Some("Mutate explicit references via MCP".into())),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?;
        let semantic_no_op = prepared.node.fields().revision_hash == fields.revision_hash;
        let status = if semantic_no_op {
            MutationStatus::NoOp
        } else if params.options.dry_run {
            MutationStatus::Planned
        } else {
            match store
                .set_explicit_references(prepared.change(fields.version), &references)
                .map_err(crate::store_error)?
            {
                mdtree_sqlite::MutationOutcome::Applied => MutationStatus::Applied,
                mdtree_sqlite::MutationOutcome::NoOp => MutationStatus::NoOp,
            }
        };
        let mut warnings = post_validation(
            &store,
            params.options.validate_after_write && status == MutationStatus::Applied,
        )?;
        warnings.push(format!("affected_references:{}", references.len()));
        let result = mutation_result(
            &prepared,
            status,
            if semantic_no_op {
                fields.version
            } else {
                fields.version + 1
            },
            store.canonical_path(id).map_err(crate::store_error)?,
            warnings,
        )?;
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "set_references",
            &payload_hash,
            &result,
            now,
        )?;
        crate::json_result(result)
    }

    /// Restores an immutable revision as a new canonical head.
    #[tool(description = "Restore an immutable MDTree revision as a new head")]
    async fn restore_version(
        &self,
        Parameters(params): Parameters<RestoreVersionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload_hash = mutation_payload_hash(&params)?;
        let mut store = self.store()?;
        if let Some(result) = replay_receipt::<MutationResult>(
            &store,
            params.operation_id.as_ref(),
            "restore_version",
            &payload_hash,
        )? {
            return crate::json_result(result);
        }
        params.precondition.validate().map_err(crate::invalid)?;
        let id = Self::id_in(&store, &params.selector)?;
        let current = store
            .get(id)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("node not found"))?;
        let fields = current.fields();
        validate_precondition(
            id,
            fields.version,
            fields.content_hash.as_bytes(),
            &params.precondition,
        )?;
        let snapshot = store
            .revision(id, params.target_version)
            .map_err(crate::store_error)?
            .ok_or_else(|| crate::invalid("target revision not found"))?;
        let now = SystemClock.now_millis();
        let prepared = prepare_node_mutation(
            NodeMutationDraft {
                id,
                parent_id: snapshot.parent_id,
                slug: snapshot.slug,
                metadata: snapshot.metadata,
                markdown_content: snapshot.markdown_content,
                sibling_order: snapshot.sibling_order,
                version: fields.version + 1,
                created_at: fields.created_at,
                updated_at: now,
                created_by: params.options.author.clone(),
                change_summary: Some(format!("Restore version {}", params.target_version)),
            },
            &SystemUlidGenerator,
        )
        .map_err(crate::store_error)?;
        let semantic_no_op = prepared.node.fields().revision_hash == fields.revision_hash;
        let status = if semantic_no_op {
            MutationStatus::NoOp
        } else if params.options.dry_run {
            MutationStatus::Planned
        } else {
            match store
                .restore_version(
                    id,
                    params.target_version,
                    fields.version,
                    now,
                    params.options.author,
                    &SystemUlidGenerator,
                )
                .map_err(crate::store_error)?
            {
                mdtree_sqlite::MutationOutcome::Applied => MutationStatus::Applied,
                mdtree_sqlite::MutationOutcome::NoOp => MutationStatus::NoOp,
            }
        };
        let result = mutation_result(
            &prepared,
            status,
            if semantic_no_op {
                fields.version
            } else {
                fields.version + 1
            },
            store.canonical_path(id).map_err(crate::store_error)?,
            post_validation(
                &store,
                params.options.validate_after_write && status == MutationStatus::Applied,
            )?,
        )?;
        record_receipt(
            &store,
            params.operation_id.as_ref(),
            "restore_version",
            &payload_hash,
            &result,
            now,
        )?;
        crate::json_result(result)
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_mcp_batch(
    store: &mdtree_sqlite::SqliteStore,
    params: &MutationBatchParams,
    now: u64,
) -> Result<Vec<PreparedBatchOperation>, ErrorData> {
    let mut labels = BTreeMap::<String, NodeId>::new();
    let mut planned_slugs = BTreeMap::<NodeId, Vec<Slug>>::new();
    let mut result = Vec::with_capacity(params.operations.len());
    for operation in &params.operations {
        match operation {
            MutationBatchOperationParams::Create {
                label,
                parent,
                title,
                content,
                requested_id,
                sibling_order,
            } => {
                let parent_id = mcp_batch_id(store, &labels, parent)?;
                let id = requested_id
                    .as_deref()
                    .map(NodeId::from_str)
                    .transpose()
                    .map_err(|error| crate::invalid(error.to_string()))?
                    .unwrap_or_else(|| NodeId::new(SystemUlidGenerator.generate()));
                if let Some(label) = label {
                    if label.trim().is_empty() || labels.insert(label.clone(), id).is_some() {
                        return Err(crate::invalid(format!(
                            "batch create label is blank or duplicated: {label}"
                        )));
                    }
                }
                let existing = if store.get(parent_id).map_err(crate::store_error)?.is_some() {
                    store.children(parent_id).map_err(crate::store_error)?
                } else {
                    Vec::new()
                };
                let mut slugs = existing
                    .iter()
                    .map(|node| node.fields().slug.clone())
                    .collect::<Vec<_>>();
                slugs.extend(planned_slugs.get(&parent_id).cloned().unwrap_or_default());
                let slug = generate_slug(title, slugs.iter());
                planned_slugs
                    .entry(parent_id)
                    .or_default()
                    .push(slug.clone());
                let order = sibling_order.unwrap_or_else(|| {
                    u32::try_from(existing.len() + planned_slugs[&parent_id].len() - 1)
                        .unwrap_or(u32::MAX)
                });
                result.push(PreparedBatchOperation::Create(
                    prepare_node_mutation(
                        NodeMutationDraft {
                            id,
                            parent_id: Some(parent_id),
                            slug,
                            metadata: NodeMetadata::new(title),
                            markdown_content: content
                                .clone()
                                .unwrap_or_else(|| format!("# {title}\n")),
                            sibling_order: order,
                            version: 1,
                            created_at: now,
                            updated_at: now,
                            created_by: params.options.author.clone(),
                            change_summary: params
                                .options
                                .change_summary
                                .clone()
                                .or_else(|| Some("Mutation batch create via MCP".into())),
                        },
                        &SystemUlidGenerator,
                    )
                    .map_err(crate::store_error)?,
                ));
            }
            MutationBatchOperationParams::Update {
                selector,
                content,
                precondition,
            } => {
                let current = mcp_batch_current(store, &labels, selector, precondition)?;
                let f = current.fields();
                result.push(PreparedBatchOperation::Replace {
                    prepared: prepare_node_mutation(
                        NodeMutationDraft {
                            id: current.id(),
                            parent_id: current.parent_id(),
                            slug: f.slug.clone(),
                            metadata: f.metadata.clone(),
                            markdown_content: content.clone(),
                            sibling_order: f.sibling_order,
                            version: f.version + 1,
                            created_at: f.created_at,
                            updated_at: now,
                            created_by: params.options.author.clone(),
                            change_summary: params
                                .options
                                .change_summary
                                .clone()
                                .or_else(|| Some("Mutation batch update via MCP".into())),
                        },
                        &SystemUlidGenerator,
                    )
                    .map_err(crate::store_error)?,
                    expected_version: f.version,
                });
            }
            MutationBatchOperationParams::Rename {
                selector,
                title,
                precondition,
            } => {
                let current = mcp_batch_current(store, &labels, selector, precondition)?;
                let f = current.fields();
                let mut metadata = f.metadata.clone();
                metadata.title.clone_from(title);
                let slug = if let Some(parent) = current.parent_id() {
                    store
                        .next_child_placement(parent, title)
                        .map_err(crate::store_error)?
                        .0
                } else {
                    generate_slug(title, std::iter::empty::<&Slug>())
                };
                result.push(PreparedBatchOperation::Replace {
                    prepared: prepare_node_mutation(
                        NodeMutationDraft {
                            id: current.id(),
                            parent_id: current.parent_id(),
                            slug,
                            metadata,
                            markdown_content: f.markdown_content.clone(),
                            sibling_order: f.sibling_order,
                            version: f.version + 1,
                            created_at: f.created_at,
                            updated_at: now,
                            created_by: params.options.author.clone(),
                            change_summary: params
                                .options
                                .change_summary
                                .clone()
                                .or_else(|| Some("Mutation batch rename via MCP".into())),
                        },
                        &SystemUlidGenerator,
                    )
                    .map_err(crate::store_error)?,
                    expected_version: f.version,
                });
            }
            MutationBatchOperationParams::Move {
                selector,
                destination_parent,
                sibling_order,
                precondition,
            } => {
                let current = mcp_batch_current(store, &labels, selector, precondition)?;
                let parent = mcp_batch_id(store, &labels, destination_parent)?;
                let f = current.fields();
                let (slug, order) = if store.get(parent).map_err(crate::store_error)?.is_some() {
                    store
                        .next_child_placement(parent, &f.metadata.title)
                        .map_err(crate::store_error)?
                } else {
                    (
                        generate_slug(&f.metadata.title, std::iter::empty::<&Slug>()),
                        0,
                    )
                };
                result.push(PreparedBatchOperation::Replace {
                    prepared: prepare_node_mutation(
                        NodeMutationDraft {
                            id: current.id(),
                            parent_id: Some(parent),
                            slug,
                            metadata: f.metadata.clone(),
                            markdown_content: f.markdown_content.clone(),
                            sibling_order: sibling_order.unwrap_or(order),
                            version: f.version + 1,
                            created_at: f.created_at,
                            updated_at: now,
                            created_by: params.options.author.clone(),
                            change_summary: params
                                .options
                                .change_summary
                                .clone()
                                .or_else(|| Some("Mutation batch move via MCP".into())),
                        },
                        &SystemUlidGenerator,
                    )
                    .map_err(crate::store_error)?,
                    expected_version: f.version,
                });
            }
            MutationBatchOperationParams::Reorder {
                selector,
                sibling_order,
                precondition,
            } => {
                let current = mcp_batch_current(store, &labels, selector, precondition)?;
                let f = current.fields();
                result.push(PreparedBatchOperation::Replace {
                    prepared: prepare_node_mutation(
                        NodeMutationDraft {
                            id: current.id(),
                            parent_id: current.parent_id(),
                            slug: f.slug.clone(),
                            metadata: f.metadata.clone(),
                            markdown_content: f.markdown_content.clone(),
                            sibling_order: *sibling_order,
                            version: f.version + 1,
                            created_at: f.created_at,
                            updated_at: now,
                            created_by: params.options.author.clone(),
                            change_summary: params
                                .options
                                .change_summary
                                .clone()
                                .or_else(|| Some("Mutation batch reorder via MCP".into())),
                        },
                        &SystemUlidGenerator,
                    )
                    .map_err(crate::store_error)?,
                    expected_version: f.version,
                });
            }
            MutationBatchOperationParams::Remove {
                selector,
                expected_version,
                confirm,
            } => {
                if !confirm {
                    return Err(crate::invalid("batch removal requires confirm=true"));
                }
                result.push(PreparedBatchOperation::Remove(AtomicTreeRemoval {
                    node_id: mcp_batch_id(store, &labels, selector)?,
                    expected_version: *expected_version,
                }));
            }
            MutationBatchOperationParams::SetReferences {
                selector,
                references,
                precondition,
            } => {
                let current = mcp_batch_current(store, &labels, selector, precondition)?;
                let f = current.fields();
                let mut explicit = Vec::with_capacity(references.len());
                for input in references {
                    let reference_type = ReferenceType::from_str(&input.reference_type)
                        .map_err(|error| crate::invalid(error.to_string()))?;
                    let target = if let Some(id) = labels.get(&input.target).copied() {
                        ReferenceTarget::Resolved {
                            node_id: id,
                            target_ref: Some(input.target.clone()),
                            anchor: input.anchor.clone(),
                        }
                    } else {
                        match store
                            .resolve_reference(&input.target)
                            .map_err(crate::store_error)?
                        {
                            mdtree_sqlite::ReferenceResolution::Resolved { node_id, .. } => {
                                ReferenceTarget::Resolved {
                                    node_id,
                                    target_ref: Some(input.target.clone()),
                                    anchor: input.anchor.clone(),
                                }
                            }
                            _ => ReferenceTarget::Unresolved {
                                target_ref: input.target.clone(),
                            },
                        }
                    };
                    explicit.push(Reference {
                        source_node_id: current.id(),
                        source_section_id: None,
                        reference_type,
                        target,
                        origin: ReferenceOrigin::Explicit,
                        metadata: input.metadata.clone(),
                    });
                }
                let mut metadata = f.metadata.clone();
                metadata.extensions.insert(
                    "explicit_relations".into(),
                    serde_json::to_value(&explicit).map_err(crate::json_error)?,
                );
                result.push(PreparedBatchOperation::SetReferences {
                    prepared: prepare_node_mutation(
                        NodeMutationDraft {
                            id: current.id(),
                            parent_id: current.parent_id(),
                            slug: f.slug.clone(),
                            metadata,
                            markdown_content: f.markdown_content.clone(),
                            sibling_order: f.sibling_order,
                            version: f.version + 1,
                            created_at: f.created_at,
                            updated_at: now,
                            created_by: params.options.author.clone(),
                            change_summary: params
                                .options
                                .change_summary
                                .clone()
                                .or_else(|| Some("Mutation batch references via MCP".into())),
                        },
                        &SystemUlidGenerator,
                    )
                    .map_err(crate::store_error)?,
                    expected_version: f.version,
                    references: explicit,
                });
            }
        }
    }
    Ok(result)
}

fn mcp_batch_id(
    store: &mdtree_sqlite::SqliteStore,
    labels: &BTreeMap<String, NodeId>,
    selector: &str,
) -> Result<NodeId, ErrorData> {
    labels
        .get(selector)
        .copied()
        .map_or_else(|| MdtreeServer::id_in(store, selector), Ok)
}

fn mcp_batch_current(
    store: &mdtree_sqlite::SqliteStore,
    labels: &BTreeMap<String, NodeId>,
    selector: &str,
    precondition: &WritePrecondition,
) -> Result<mdtree_core::Node, ErrorData> {
    precondition.validate().map_err(crate::invalid)?;
    let id = mcp_batch_id(store, labels, selector)?;
    let current = store
        .get(id)
        .map_err(crate::store_error)?
        .ok_or_else(|| crate::invalid("batch operation requires an existing node"))?;
    validate_precondition(
        id,
        current.fields().version,
        current.fields().content_hash.as_bytes(),
        precondition,
    )?;
    Ok(current)
}

#[allow(clippy::too_many_arguments)]
fn prepare_create_branch(
    store: &mdtree_sqlite::SqliteStore,
    parent_id: NodeId,
    parent_path: &[Slug],
    title: &str,
    content: Option<&str>,
    metadata: Option<&serde_json::Value>,
    requested_id: Option<&str>,
    sibling_order: Option<u32>,
    children: &[CreateNodeChildParams],
    options: &MutationOptions,
    now: u64,
    depth: usize,
    sibling_slugs: &mut Vec<Slug>,
    next_order: &mut u32,
    ids: &mut BTreeSet<NodeId>,
    prepared: &mut Vec<PreparedNodeMutation>,
    paths: &mut Vec<Vec<Slug>>,
) -> Result<(), ErrorData> {
    if depth > MAX_CREATE_NODE_DEPTH {
        return Err(crate::invalid(format!(
            "create_node descendants exceed maximum depth {MAX_CREATE_NODE_DEPTH}"
        )));
    }
    if prepared.len() >= MAX_CREATE_NODE_COUNT {
        return Err(crate::invalid(format!(
            "create_node request exceeds maximum node count {MAX_CREATE_NODE_COUNT}"
        )));
    }

    let slug = generate_slug(title, sibling_slugs.iter());
    sibling_slugs.push(slug.clone());
    let order = sibling_order.unwrap_or(*next_order);
    *next_order = (*next_order).max(order.saturating_add(1));
    let id = requested_id
        .map(NodeId::from_str)
        .transpose()
        .map_err(|error| crate::invalid(error.to_string()))?
        .unwrap_or_else(|| NodeId::new(SystemUlidGenerator.generate()));
    if !ids.insert(id) || store.get(id).map_err(crate::store_error)?.is_some() {
        return Err(crate::invalid(format!("node already exists: {id}")));
    }
    let mut metadata = metadata
        .cloned()
        .map(serde_json::from_value::<NodeMetadata>)
        .transpose()
        .map_err(|error| crate::invalid(format!("invalid metadata: {error}")))?
        .unwrap_or_else(|| NodeMetadata::new(title));
    title.clone_into(&mut metadata.title);
    let mut path = parent_path.to_vec();
    path.push(slug.clone());
    let node = prepare_node_mutation(
        NodeMutationDraft {
            id,
            parent_id: Some(parent_id),
            slug,
            metadata,
            markdown_content: content.map_or_else(|| format!("# {title}\n"), str::to_owned),
            sibling_order: order,
            version: 1,
            created_at: now,
            updated_at: now,
            created_by: options.author.clone(),
            change_summary: options
                .change_summary
                .clone()
                .or_else(|| Some("Create node via MCP".into())),
        },
        &SystemUlidGenerator,
    )
    .map_err(crate::store_error)?;
    prepared.push(node);
    paths.push(path.clone());

    let mut child_slugs = Vec::new();
    let mut child_order = 0;
    for child in children {
        prepare_create_branch(
            store,
            id,
            &path,
            &child.title,
            child.content.as_deref(),
            child.metadata.as_ref(),
            child.requested_id.as_deref(),
            child.sibling_order,
            &child.children,
            options,
            now,
            depth + 1,
            &mut child_slugs,
            &mut child_order,
            ids,
            prepared,
            paths,
        )?;
    }
    Ok(())
}

fn hash_hex(bytes: &[u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

fn mutation_result(
    prepared: &PreparedNodeMutation,
    status: MutationStatus,
    version: u64,
    path: Vec<mdtree_core::Slug>,
    warnings: Vec<String>,
) -> Result<MutationResult, ErrorData> {
    let fields = prepared.node.fields();
    Ok(MutationResult {
        status,
        node_id: prepared.node.id().to_string(),
        version,
        content_hash: hash_hex(fields.content_hash.as_bytes()),
        revision_hash: hash_hex(fields.revision_hash.as_bytes()),
        path: path
            .into_iter()
            .map(|slug| slug.to_string())
            .collect::<Vec<_>>()
            .join("/"),
        node: MutationNodeResult {
            id: prepared.node.id().to_string(),
            parent_id: prepared
                .node
                .parent_id()
                .map_or_else(String::new, |parent| parent.to_string()),
            slug: fields.slug.to_string(),
            metadata: serde_json::to_value(&fields.metadata).map_err(crate::json_error)?,
            content: fields.markdown_content.clone(),
            sibling_order: fields.sibling_order,
        },
        warnings,
    })
}

fn post_validation(
    store: &mdtree_sqlite::SqliteStore,
    enabled: bool,
) -> Result<Vec<String>, ErrorData> {
    if !enabled {
        return Ok(Vec::new());
    }
    Ok(store
        .validate_integrity()
        .map_err(crate::store_error)?
        .findings
        .into_iter()
        .take(100)
        .map(|finding| {
            finding.node_id.map_or_else(
                || format!("validation:{}:{}", finding.code, finding.detail),
                |id| format!("validation:{}:{id}:{}", finding.code, finding.detail),
            )
        })
        .collect())
}

fn validate_precondition(
    id: NodeId,
    actual_version: u64,
    actual_hash: &[u8; 32],
    precondition: &WritePrecondition,
) -> Result<(), ErrorData> {
    let version_matches = precondition
        .expected_version
        .is_none_or(|expected| expected == actual_version);
    let actual_hash = hash_hex(actual_hash);
    let hash_matches = precondition
        .expected_content_hash
        .as_deref()
        .is_none_or(|expected| expected == actual_hash);
    if version_matches && hash_matches {
        Ok(())
    } else {
        Err(precondition_conflict(
            id,
            precondition.expected_version,
            actual_version,
        ))
    }
}

fn precondition_conflict(id: NodeId, expected: Option<u64>, actual: u64) -> ErrorData {
    let detail = MutationErrorDetail {
        code: "conflict".into(),
        node_id: Some(id.to_string()),
        expected_version: expected,
        actual_version: Some(actual),
    };
    ErrorData::invalid_params(
        format!("mutation precondition does not match node {id}"),
        serde_json::to_value(detail).ok(),
    )
}

fn mutation_payload_hash(value: &impl Serialize) -> Result<[u8; 32], ErrorData> {
    let payload = serde_json::to_string(value).map_err(crate::json_error)?;
    Ok(*mdtree_core::hash_content(&payload).as_bytes())
}

fn replay_receipt<T: DeserializeOwned>(
    store: &mdtree_sqlite::SqliteStore,
    operation_id: Option<&String>,
    tool_name: &str,
    payload_hash: &[u8; 32],
) -> Result<Option<T>, ErrorData> {
    let Some(operation_id) = operation_id else {
        return Ok(None);
    };
    if operation_id.trim().is_empty() {
        return Err(crate::invalid("operation_id must not be blank"));
    }
    store
        .mutation_receipt(operation_id, tool_name, payload_hash)
        .map_err(crate::store_error)?
        .map(|json| serde_json::from_str(&json).map_err(crate::json_error))
        .transpose()
}

fn record_receipt(
    store: &mdtree_sqlite::SqliteStore,
    operation_id: Option<&String>,
    tool_name: &str,
    payload_hash: &[u8; 32],
    result: &impl Serialize,
    created_at: u64,
) -> Result<(), ErrorData> {
    let Some(operation_id) = operation_id else {
        return Ok(());
    };
    let json = serde_json::to_string(result).map_err(crate::json_error)?;
    store
        .record_mutation_receipt(operation_id, tool_name, payload_hash, &json, created_at)
        .map_err(crate::store_error)
}

#[cfg(test)]
mod tests {
    use mdtree_core::NodeId;

    use super::{
        CreateNodeParams, McpAccessMode, MutationErrorDetail, MutationOptions, WritePrecondition,
    };

    #[test]
    fn shared_contracts_have_stable_json_shapes() {
        let precondition = WritePrecondition {
            expected_version: Some(7),
            expected_content_hash: None,
        };
        let options = MutationOptions {
            dry_run: true,
            author: Some("agent".into()),
            change_summary: Some("preview".into()),
            validate_after_write: true,
        };
        assert_eq!(
            serde_json::to_value(&precondition).expect("precondition"),
            serde_json::json!({"expected_version":7,"expected_content_hash":null})
        );
        assert_eq!(
            serde_json::to_value(options).expect("options"),
            serde_json::json!({"dry_run":true,"author":"agent","change_summary":"preview","validate_after_write":true})
        );
        let single_create = CreateNodeParams {
            operation_id: None,
            parent: "root".into(),
            title: "Child".into(),
            content: None,
            metadata: None,
            requested_id: None,
            sibling_order: None,
            children: Vec::new(),
            options: MutationOptions::default(),
        };
        assert_eq!(
            serde_json::to_value(single_create).expect("single create"),
            serde_json::json!({
                "operation_id":null,
                "parent":"root",
                "title":"Child",
                "content":null,
                "metadata":null,
                "requested_id":null,
                "sibling_order":null,
                "options":{
                    "dry_run":false,
                    "author":null,
                    "change_summary":null,
                    "validate_after_write":false
                }
            })
        );
        assert!(!McpAccessMode::ReadOnly.allows_write());
        assert!(McpAccessMode::ReadWrite.allows_write());
        assert!(precondition.validate().is_ok());
        assert!(WritePrecondition::default().validate().is_err());
        assert!(WritePrecondition {
            expected_version: None,
            expected_content_hash: Some("A".repeat(64)),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn conflicts_map_to_stable_structured_details() {
        let node_id: NodeId = "01JZ8Q5CWPN8T7KPN5A1V9B6XM".parse().expect("ID");
        let detail = MutationErrorDetail::from(&mdtree_sqlite::StoreError::Conflict {
            node_id,
            expected: 2,
            actual: 3,
        });
        assert_eq!(detail.code, "conflict");
        assert_eq!(detail.node_id, Some(node_id.to_string()));
        assert_eq!(detail.expected_version, Some(2));
        assert_eq!(detail.actual_version, Some(3));
    }
}
