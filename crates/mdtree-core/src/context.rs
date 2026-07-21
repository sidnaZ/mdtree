//! Bounded subtree-inspection and agent-context response contracts.

use serde::{Deserialize, Serialize};

use crate::{Breadcrumb, NodeHash, NodeId, NodeType, PageCursor, Reference};

/// Compact structural node summary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextSummary {
    /// Stable node identity.
    pub node_id: NodeId,
    /// Current title.
    pub title: String,
    /// Optional concise summary.
    pub summary: Option<String>,
    /// Optional node type.
    pub node_type: Option<NodeType>,
    /// Current breadcrumb.
    pub breadcrumb: Breadcrumb,
}

/// One bounded subtree-inspection row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InspectionItem {
    /// Compact node state.
    pub node: ContextSummary,
    /// Depth relative to the inspected root.
    pub depth: u32,
    /// Immediate child count.
    pub child_count: u32,
}

/// Structural summary plus routing conventions inherited by writers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConventionSummary {
    /// Compact ancestor state.
    pub node: ContextSummary,
    /// Canonical concepts owned by the ancestor subtree.
    pub owns: Vec<String>,
    /// Concepts explicitly routed elsewhere.
    pub excludes: Vec<String>,
}

/// Deterministically bounded tree view.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubtreeInspection {
    /// Included nodes in stable tree order.
    pub items: Vec<InspectionItem>,
    /// Opaque token for the next bounded window, absent when no eligible items remain.
    pub next_cursor: Option<PageCursor>,
    /// Whether depth or item limits omitted eligible nodes.
    pub truncated: bool,
}

/// Read-oriented context assembled in documented priority order.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReadContext {
    /// Selected node.
    pub node: ContextSummary,
    /// Root and ancestor summaries, root first.
    pub ancestors: Vec<ContextSummary>,
    /// Current canonical Markdown, when budget permits.
    pub content: Option<String>,
    /// Immediate child summaries.
    pub children: Vec<ContextSummary>,
    /// Relevant outgoing references.
    pub references: Vec<Reference>,
    /// Categories omitted to honor the hard budget.
    pub omitted: Vec<String>,
    /// Whether any material was omitted.
    pub truncated: bool,
    /// Ceiling byte count divided by four; a documented rough token estimate.
    pub estimated_tokens: u64,
}

/// Write-oriented context sufficient for an optimistic mutation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WriteContext {
    /// Selected target.
    pub target: ContextSummary,
    /// Required expected version.
    pub version: u64,
    /// Required expected content hash.
    pub content_hash: NodeHash,
    /// Target ownership conventions.
    pub owns: Vec<String>,
    /// Target exclusions.
    pub excludes: Vec<String>,
    /// Accepted child types.
    pub accepts_children: Vec<NodeType>,
    /// Nearby sibling examples.
    pub sibling_examples: Vec<ContextSummary>,
    /// Ancestor ownership and exclusion guidance.
    pub ancestor_conventions: Vec<ConventionSummary>,
    /// Relevant outgoing references.
    pub references: Vec<Reference>,
    /// Categories omitted to honor the hard budget.
    pub omitted: Vec<String>,
    /// Whether any material was omitted.
    pub truncated: bool,
    /// Ceiling byte count divided by four; a documented rough token estimate.
    pub estimated_tokens: u64,
}
