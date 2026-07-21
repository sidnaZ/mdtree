//! Versioned interoperable workspace snapshot contracts.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::{NodeHash, NodeId, NodeMetadata, NodeRevision, Reference, Slug};

/// Latest JSON/Markdown snapshot format understood by this executable.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// Explicit revision retention policy represented by a snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionPolicy {
    /// Only canonical heads are present.
    HeadOnly,
    /// Complete immutable revision history is present.
    Complete,
}

/// Workspace-level snapshot metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotWorkspace {
    /// Human-readable workspace name.
    pub name: String,
    /// Canonical database-format version represented by the snapshot.
    pub workspace_format_version: u32,
}

/// Complete canonical head state for one node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SnapshotNode {
    /// Stable identity preserved across export/import.
    pub id: NodeId,
    /// Canonical parent, absent for the root.
    pub parent_id: Option<NodeId>,
    /// Canonical slug.
    pub slug: Slug,
    /// Complete metadata.
    pub metadata: NodeMetadata,
    /// Canonical Markdown content.
    pub markdown_content: String,
    /// Deterministic sibling order.
    pub sibling_order: u32,
    /// Current optimistic-concurrency version.
    pub version: u64,
    /// Exact content hash.
    pub content_hash: NodeHash,
    /// Semantic revision hash.
    pub revision_hash: NodeHash,
    /// Original creation time in Unix milliseconds.
    pub created_at: u64,
    /// Last mutation time in Unix milliseconds.
    pub updated_at: u64,
}

/// Complete versioned workspace snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Snapshot {
    /// Stable format discriminator.
    pub format: String,
    /// Snapshot format version.
    pub format_version: u32,
    /// Workspace metadata.
    pub workspace: SnapshotWorkspace,
    /// Revision retention policy.
    pub revision_policy: RevisionPolicy,
    /// Canonical node heads.
    pub nodes: Vec<SnapshotNode>,
    /// Retained immutable revisions.
    pub revisions: Vec<NodeRevision>,
    /// Typed explicit/imported relationships and unresolved targets.
    pub references: Vec<Reference>,
}

/// One actionable snapshot validation error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotValidationError {
    /// Stable machine-readable code.
    pub code: String,
    /// Optional affected node.
    pub node_id: Option<NodeId>,
    /// Actionable detail.
    pub message: String,
}

/// Aggregate validation result produced before any mutation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotValidationReport {
    /// Every discovered validation problem.
    pub errors: Vec<SnapshotValidationError>,
}

impl SnapshotValidationReport {
    /// Whether the snapshot can be imported.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validates complete snapshot structure and reports every actionable error.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn validate_snapshot(snapshot: &Snapshot) -> SnapshotValidationReport {
    let mut report = SnapshotValidationReport::default();
    let mut push = |code: &str, node_id: Option<NodeId>, message: String| {
        report.errors.push(SnapshotValidationError {
            code: code.into(),
            node_id,
            message,
        });
    };
    if snapshot.format != "mdtree-snapshot" {
        push(
            "format",
            None,
            format!("unsupported format {}", snapshot.format),
        );
    }
    if snapshot.format_version != SNAPSHOT_FORMAT_VERSION {
        push(
            "format_version",
            None,
            format!("unsupported snapshot version {}", snapshot.format_version),
        );
    }
    if snapshot.workspace.name.trim().is_empty() {
        push(
            "workspace_name",
            None,
            "workspace name must not be blank".into(),
        );
    }
    if snapshot.nodes.is_empty() {
        push("nodes", None, "snapshot must contain nodes".into());
    }
    let mut nodes = HashMap::new();
    for node in &snapshot.nodes {
        if nodes.insert(node.id, node).is_some() {
            push("duplicate_id", Some(node.id), "duplicate node ID".into());
        }
        if node.metadata.title.trim().is_empty() {
            push("title", Some(node.id), "title must not be blank".into());
        }
        if node.version == 0 {
            push(
                "version",
                Some(node.id),
                "version must be at least 1".into(),
            );
        }
        if node.updated_at < node.created_at {
            push(
                "timestamps",
                Some(node.id),
                "updated_at precedes created_at".into(),
            );
        }
    }
    let roots: Vec<_> = snapshot
        .nodes
        .iter()
        .filter(|node| node.parent_id.is_none())
        .collect();
    if roots.len() != 1 {
        push(
            "root_count",
            None,
            format!("expected one root, found {}", roots.len()),
        );
    }
    let mut sibling_keys = HashSet::new();
    for node in &snapshot.nodes {
        if let Some(parent) = node.parent_id {
            if !nodes.contains_key(&parent) {
                push(
                    "missing_parent",
                    Some(node.id),
                    format!("parent {parent} is absent"),
                );
            }
        } else if node.sibling_order != 0 {
            push(
                "root_order",
                Some(node.id),
                "root sibling order must be 0".into(),
            );
        }
        if !sibling_keys.insert((node.parent_id, node.slug.as_str())) {
            push(
                "duplicate_sibling_slug",
                Some(node.id),
                format!("duplicate slug {} below the same parent", node.slug),
            );
        }
        let mut seen = HashSet::new();
        let mut current = Some(node.id);
        while let Some(id) = current {
            if !seen.insert(id) {
                push("cycle", Some(node.id), "parent cycle detected".into());
                break;
            }
            current = nodes.get(&id).and_then(|item| item.parent_id);
        }
    }
    let ids: HashSet<_> = snapshot.nodes.iter().map(|node| node.id).collect();
    for reference in &snapshot.references {
        if !ids.contains(&reference.source_node_id) {
            push(
                "reference_source",
                Some(reference.source_node_id),
                "reference source is absent".into(),
            );
        }
        if let crate::ReferenceTarget::Resolved { node_id, .. } = &reference.target {
            if !ids.contains(node_id) {
                push(
                    "reference_target",
                    Some(reference.source_node_id),
                    format!("resolved target {node_id} is absent"),
                );
            }
        }
    }
    let mut versions = HashSet::new();
    for revision in &snapshot.revisions {
        if !ids.contains(&revision.node_id) {
            push(
                "revision_node",
                Some(revision.node_id),
                "revision node is absent".into(),
            );
        }
        if !versions.insert((revision.node_id, revision.version)) {
            push(
                "duplicate_revision",
                Some(revision.node_id),
                format!("duplicate revision {}", revision.version),
            );
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::{validate_snapshot, Snapshot};
    #[test]
    fn northstar_platform_example_deserializes_and_validates() {
        let snapshot: Snapshot = serde_json::from_str(include_str!(
            "../../../examples/northstar-platform.snapshot.json"
        ))
        .expect("example JSON");
        assert!(validate_snapshot(&snapshot).is_valid());
    }
}
