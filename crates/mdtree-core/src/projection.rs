//! Targeted read-projection contracts shared by persistence and delivery adapters.

use serde::{Deserialize, Serialize};

use crate::{ContextSummary, Node, NodeHash, NodeId, Page, SnapshotNode};

/// Maximum selectors accepted by one bounded batch read.
pub const MAX_BATCH_ITEMS: usize = 100;
/// Maximum parent groups accepted by one child batch.
pub const MAX_BATCH_PARENTS: usize = 20;
/// Maximum total requested child items across one batch.
pub const MAX_BATCH_CHILDREN: u32 = 100;

/// Stable per-item batch lookup failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchLookupError {
    /// Machine-readable category.
    pub code: String,
    /// Human-readable detail scoped to this item.
    pub message: String,
}

/// One ordered result from a bounded multi-selector node lookup.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BatchNodeLookupItem {
    /// Original selector exactly as supplied by the caller.
    pub selector: String,
    /// Canonical node when lookup succeeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeProjection>,
    /// Stable per-selector error when lookup fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchLookupError>,
}

/// One requested parent page in a bounded batch child lookup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchChildrenRequest {
    /// Parent selector in caller-provided form.
    pub parent: String,
    /// Requested page size for this parent.
    pub limit: u32,
    /// Optional opaque cursor previously returned for this same parent.
    pub cursor: Option<String>,
}

/// One grouped result from a bounded batch child lookup.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BatchChildrenLookupItem {
    /// Original parent selector.
    pub parent: String,
    /// Canonical child page when lookup succeeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<Page<NodeProjection>>,
    /// Stable per-parent error when lookup fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchLookupError>,
}

/// Shared atomic subtree-clone service input after selector resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloneSubtreeRequest {
    /// Existing subtree root.
    pub source_id: NodeId,
    /// Existing destination parent.
    pub destination_parent_id: NodeId,
    /// Caller-observed source version.
    pub expected_version: u64,
    /// Optional zero-based destination placement.
    pub sibling_order: Option<u32>,
    /// Validate and plan without writing.
    pub dry_run: bool,
    /// Operation timestamp in Unix milliseconds.
    pub created_at: u64,
    /// Optional immutable revision author.
    pub created_by: Option<String>,
    /// Optional immutable revision explanation.
    pub change_summary: Option<String>,
}

/// Planned or applied atomic subtree-clone summary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CloneSubtreeResult {
    /// `planned` or `applied`.
    pub status: String,
    /// Original subtree root.
    pub source_id: NodeId,
    /// Destination parent.
    pub destination_parent_id: NodeId,
    /// New root identity, present only after application.
    pub cloned_root_id: Option<NodeId>,
    /// Collision-safe cloned-root slug.
    pub root_slug: String,
    /// Applied or planned destination order.
    pub sibling_order: u32,
    /// Number of cloned nodes.
    pub node_count: u32,
    /// Number of copied explicit references.
    pub explicit_reference_count: u32,
}

/// Structural child-existence predicate applied before subtree pagination.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralPredicate {
    /// Nodes without canonical children.
    Leaf,
    /// Nodes with at least one canonical child.
    Internal,
}

impl StructuralPredicate {
    /// Stable cursor/request key value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leaf => "leaf",
            Self::Internal => "internal",
        }
    }
}

/// Complete canonical head state returned by a targeted read.
///
/// This alias intentionally preserves the established snapshot-node fields and
/// serialization while allowing targeted reads to avoid constructing a complete
/// workspace snapshot.
pub type NodeProjection = SnapshotNode;

/// Compact canonical summary returned by a targeted read.
pub type NodeSummaryProjection = ContextSummary;

/// One targeted canonical node paired with its relative traversal depth.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NodeDepthProjection {
    /// Relative depth supplied by the traversal query.
    pub depth: u32,
    /// Complete canonical head state.
    pub node: NodeProjection,
}

/// Aggregate immediate-child count for one requested node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeChildCount {
    /// Requested stable node identity.
    pub node_id: NodeId,
    /// Number of direct canonical children.
    pub child_count: u32,
}

/// Compact immutable-history entry returned by revision listings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RevisionSummary {
    /// Stable node identity shared by every retained revision.
    pub node_id: NodeId,
    /// Monotonically increasing immutable version.
    pub version: u64,
    /// Optional human or agent identity responsible for the revision.
    pub created_by: Option<String>,
    /// Revision creation time in Unix epoch milliseconds.
    pub created_at: u64,
    /// Optional human-readable mutation summary.
    pub change_summary: Option<String>,
    /// Hash of Markdown content at this version.
    pub content_hash: NodeHash,
    /// Hash of the complete canonical snapshot at this version.
    pub revision_hash: NodeHash,
}

/// Compact database-side aggregates for one canonical subtree.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TreeStatistics {
    /// Selected subtree root.
    pub node_id: NodeId,
    /// Immediate canonical children of the selected node.
    pub direct_child_count: u64,
    /// Nodes in the subtree including the selected node.
    pub subtree_size: u64,
    /// Nodes in the subtree with no canonical children.
    pub leaf_count: u64,
    /// Greatest edge distance from the selected node to a descendant.
    pub max_relative_depth: u32,
    /// Greatest number of subtree nodes at any one relative depth.
    pub max_width: u64,
}

/// Reflexive ancestor-containment result for two canonical nodes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AncestorContainment {
    /// Proposed ancestor node.
    pub ancestor_id: NodeId,
    /// Proposed descendant node.
    pub descendant_id: NodeId,
    /// True when the ancestor is the same node or lies on its root path.
    pub contains: bool,
}

/// Edge distance and LCA for two canonical nodes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TreeDistance {
    /// Oriented route origin.
    pub from_id: NodeId,
    /// Oriented route destination.
    pub to_id: NodeId,
    /// Reflexive lowest common ancestor of both endpoints.
    pub lowest_common_ancestor_id: NodeId,
    /// Number of canonical parent-child edges between endpoints.
    pub distance: u32,
}

/// Canonical endpoint-inclusive route from one node to another.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PathBetween {
    /// Oriented route origin.
    pub from_id: NodeId,
    /// Oriented route destination.
    pub to_id: NodeId,
    /// Reflexive lowest common ancestor included in the route.
    pub lowest_common_ancestor_id: NodeId,
    /// Number of canonical parent-child edges in the route.
    pub distance: u32,
    /// Nodes ordered from `from_id`, upward through the LCA, then down to `to_id`.
    pub nodes: Vec<NodeProjection>,
}

/// Zero-based canonical child lookup result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IndexedChild {
    /// Selected canonical parent.
    pub parent_id: NodeId,
    /// Requested zero-based canonical position.
    pub index: u32,
    /// Child at the requested position, absent when the index is out of range.
    pub node: Option<NodeProjection>,
}

/// Direct canonical neighbors of one selected node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AdjacentSiblings {
    /// Selected canonical node.
    pub node_id: NodeId,
    /// Immediately preceding canonical sibling, absent at the first position or root.
    pub previous: Option<NodeProjection>,
    /// Immediately following canonical sibling, absent at the last position or root.
    pub next: Option<NodeProjection>,
}

/// Stable database-side traversal ordering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraversalOrder {
    /// Pre-order depth-first traversal in canonical sibling order.
    Dfs,
    /// Breadth-first traversal by relative depth, then canonical ancestry path.
    Bfs,
}

impl TraversalOrder {
    /// Stable cursor-scope name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dfs => "dfs",
            Self::Bfs => "bfs",
        }
    }
}

impl From<&Node> for SnapshotNode {
    fn from(node: &Node) -> Self {
        let fields = node.fields();
        Self {
            id: node.id(),
            parent_id: node.parent_id(),
            slug: fields.slug.clone(),
            metadata: fields.metadata.clone(),
            markdown_content: fields.markdown_content.clone(),
            sibling_order: fields.sibling_order,
            version: fields.version,
            content_hash: fields.content_hash,
            revision_hash: fields.revision_hash,
            created_at: fields.created_at,
            updated_at: fields.updated_at,
        }
    }
}

/// Projects already-loaded canonical heads in caller-provided order.
pub fn project_canonical_nodes<'a>(
    nodes: impl IntoIterator<Item = &'a Node>,
) -> Vec<NodeProjection> {
    nodes.into_iter().map(NodeProjection::from).collect()
}

#[cfg(test)]
mod tests {
    use crate::northstar_platform_snapshot;

    #[test]
    fn targeted_projection_preserves_every_canonical_field_and_hash_encoding() {
        let expected = northstar_platform_snapshot().nodes.remove(1);
        let node = crate::Node::new(
            crate::NodeFields {
                id: expected.id,
                slug: expected.slug.clone(),
                metadata: expected.metadata.clone(),
                markdown_content: expected.markdown_content.clone(),
                sibling_order: expected.sibling_order,
                version: expected.version,
                content_hash: expected.content_hash,
                revision_hash: expected.revision_hash,
                created_at: expected.created_at,
                updated_at: expected.updated_at,
            },
            expected.parent_id,
        )
        .expect("valid fixture node");

        let projected = crate::NodeProjection::from(&node);
        assert_eq!(projected, expected);
        assert_eq!(
            serde_json::to_value(projected).expect("projection JSON"),
            serde_json::to_value(expected).expect("snapshot JSON")
        );
    }
}
