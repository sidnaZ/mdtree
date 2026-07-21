//! Deterministic scale fixtures shared by persistence and delivery-adapter tests.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde_json::Value;

use crate::{
    hash_content, hash_revision, NodeId, NodeMetadata, NodeRevision, NodeType, Reference,
    ReferenceOrigin, ReferenceTarget, ReferenceType, RevisionHashInput, RevisionPolicy,
    SequentialUlidGenerator, Slug, Snapshot, SnapshotNode, SnapshotWorkspace, UlidGenerator,
    SNAPSHOT_FORMAT_VERSION,
};

/// Default one-mebibyte serialized-response boundary used by scale tests.
pub const DEFAULT_RESPONSE_BOUNDARY_BYTES: usize = 1_048_576;

/// Shape controls for the reusable composite scale fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LargeTreeFixtureSpec {
    /// Direct children below the wide branch.
    pub wide_children: usize,
    /// Descendants in the single-child deep branch.
    pub deep_descendants: usize,
    /// Immutable versions retained for the history-heavy node, including its head.
    pub history_revisions: usize,
    /// Explicit resolved relations distributed across wide nodes.
    pub relations: usize,
    /// Center point for the two content-size boundary nodes.
    pub response_boundary_bytes: usize,
}

impl LargeTreeFixtureSpec {
    /// Realistic regression shape that exceeds current structural and response boundaries.
    #[must_use]
    pub const fn regression() -> Self {
        Self {
            wide_children: 128,
            deep_descendants: 128,
            history_revisions: 64,
            relations: 256,
            response_boundary_bytes: DEFAULT_RESPONSE_BOUNDARY_BYTES,
        }
    }
}

impl Default for LargeTreeFixtureSpec {
    fn default() -> Self {
        Self::regression()
    }
}

/// Composite fixture and stable selectors for each scale-heavy region.
#[derive(Clone, Debug, PartialEq)]
pub struct LargeTreeFixture {
    /// Complete deterministic snapshot.
    pub snapshot: Snapshot,
    /// Canonical root.
    pub root_id: NodeId,
    /// Parent of the wide sibling set.
    pub wide_parent_id: NodeId,
    /// Canonically ordered wide children.
    pub wide_child_ids: Vec<NodeId>,
    /// Parent of the deep single-child chain.
    pub deep_parent_id: NodeId,
    /// Deepest node in the chain, or the branch parent when depth is zero.
    pub deep_leaf_id: NodeId,
    /// Node retaining complete immutable history.
    pub history_node_id: NodeId,
    /// Node whose Markdown is one byte below the configured boundary.
    pub below_boundary_node_id: NodeId,
    /// Node whose Markdown is one byte above the configured boundary.
    pub above_boundary_node_id: NodeId,
}

/// Generates wide, deep, history-heavy, relation-heavy, and response-boundary data.
///
/// The same seed and specification always produce byte-for-byte equal snapshots. This
/// fixture is deliberately transport-neutral so `SQLite`, CLI, and MCP tests can import
/// exactly the same canonical workspace.
///
/// # Panics
///
/// Panics only when generated constants violate domain validation or requested counts
/// exceed representable sibling orders or versions.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate_large_tree_fixture(spec: LargeTreeFixtureSpec, seed: u64) -> LargeTreeFixture {
    let generator = SequentialUlidGenerator::new(seed);
    let next_id = || NodeId::new(generator.generate());
    let root_id = next_id();
    let wide_parent_id = next_id();
    let deep_parent_id = next_id();
    let history_node_id = next_id();
    let below_boundary_node_id = next_id();
    let above_boundary_node_id = next_id();
    let mut nodes = vec![
        snapshot_node(
            root_id,
            None,
            "scale-workspace",
            "Scale Workspace",
            0,
            1,
            "# Scale Workspace\n",
        ),
        snapshot_node(
            wide_parent_id,
            Some(root_id),
            "wide",
            "Wide Branch",
            0,
            1,
            "# Wide Branch\n",
        ),
        snapshot_node(
            deep_parent_id,
            Some(root_id),
            "deep",
            "Deep Branch",
            1,
            1,
            "# Deep Branch\n",
        ),
    ];

    let generated_wide_ids = (0..spec.wide_children)
        .map(|_| next_id())
        .collect::<Vec<_>>();
    let mut wide_child_ids = Vec::with_capacity(spec.wide_children);
    for index in 0..spec.wide_children {
        let generated_index = if index % 2 == 0 {
            spec.wide_children - 1 - index / 2
        } else {
            index / 2
        };
        let id = generated_wide_ids[generated_index];
        wide_child_ids.push(id);
        nodes.push(snapshot_node(
            id,
            Some(wide_parent_id),
            &format!("wide-{index:05}"),
            &format!("Wide Node {index:05}"),
            u32::try_from(index).expect("wide sibling order"),
            1,
            &format!("# Wide Node {index:05}\n\nDeterministic wide fixture content.\n"),
        ));
    }

    let mut parent = deep_parent_id;
    for index in 0..spec.deep_descendants {
        let id = next_id();
        nodes.push(snapshot_node(
            id,
            Some(parent),
            &format!("deep-{index:05}"),
            &format!("Deep Node {index:05}"),
            0,
            1,
            &format!("# Deep Node {index:05}\n\nDeterministic deep fixture content.\n"),
        ));
        parent = id;
    }
    let deep_leaf_id = parent;

    let history_count = spec.history_revisions.max(1);
    let head_version = u64::try_from(history_count).expect("history version");
    let history_head_content = history_content(head_version);
    nodes.push(snapshot_node(
        history_node_id,
        Some(root_id),
        "history-heavy",
        "History Heavy",
        2,
        head_version,
        &history_head_content,
    ));
    let history_node = nodes.last().expect("history head").clone();
    let revisions = (1..=history_count)
        .map(|version| history_revision(&history_node, u64::try_from(version).expect("version")))
        .collect();

    nodes.push(snapshot_node(
        below_boundary_node_id,
        Some(root_id),
        "below-response-boundary",
        "Below Response Boundary",
        3,
        1,
        &sized_markdown(
            "Below Response Boundary",
            spec.response_boundary_bytes.saturating_sub(1),
        ),
    ));
    nodes.push(snapshot_node(
        above_boundary_node_id,
        Some(root_id),
        "above-response-boundary",
        "Above Response Boundary",
        4,
        1,
        &sized_markdown(
            "Above Response Boundary",
            spec.response_boundary_bytes.saturating_add(1),
        ),
    ));

    let relation_nodes = if wide_child_ids.is_empty() {
        vec![wide_parent_id]
    } else {
        wide_child_ids.clone()
    };
    let references = (0..spec.relations)
        .map(|index| {
            let source = relation_nodes[index % relation_nodes.len()];
            let target =
                relation_nodes[(index.wrapping_mul(17).wrapping_add(1)) % relation_nodes.len()];
            let mut metadata = BTreeMap::<String, Value>::new();
            metadata.insert("fixture_index".into(), Value::from(index));
            Reference {
                source_node_id: source,
                source_section_id: None,
                reference_type: ReferenceType::from_str("scale_relation").expect("relation type"),
                target: ReferenceTarget::Resolved {
                    node_id: target,
                    target_ref: Some(target.to_string()),
                    anchor: None,
                },
                origin: ReferenceOrigin::Agent,
                metadata,
            }
        })
        .collect();

    LargeTreeFixture {
        snapshot: Snapshot {
            format: "mdtree-snapshot".into(),
            format_version: SNAPSHOT_FORMAT_VERSION,
            workspace: SnapshotWorkspace {
                name: "Scale Workspace".into(),
                workspace_format_version: 1,
            },
            revision_policy: RevisionPolicy::Complete,
            nodes,
            revisions,
            references,
        },
        root_id,
        wide_parent_id,
        wide_child_ids,
        deep_parent_id,
        deep_leaf_id,
        history_node_id,
        below_boundary_node_id,
        above_boundary_node_id,
    }
}

fn snapshot_node(
    id: NodeId,
    parent_id: Option<NodeId>,
    slug: &str,
    title: &str,
    sibling_order: u32,
    version: u64,
    markdown_content: &str,
) -> SnapshotNode {
    let slug = Slug::from_str(slug).expect("fixture slug");
    let mut metadata = NodeMetadata::new(title);
    metadata.node_type = Some(NodeType::from_str("scale_fixture").expect("fixture type"));
    metadata.tags = vec!["scale".into()];
    let revision_hash = hash_revision(RevisionHashInput {
        node_id: id,
        parent_id,
        slug: &slug,
        metadata: &metadata,
        markdown_content,
        sibling_order,
    })
    .expect("fixture revision hash");
    SnapshotNode {
        id,
        parent_id,
        slug,
        metadata,
        markdown_content: markdown_content.into(),
        sibling_order,
        version,
        content_hash: hash_content(markdown_content),
        revision_hash,
        created_at: 1,
        updated_at: version,
    }
}

fn history_content(version: u64) -> String {
    format!("# History Heavy\n\nDeterministic revision {version:05}.\n")
}

fn history_revision(head: &SnapshotNode, version: u64) -> NodeRevision {
    let markdown_content = history_content(version);
    let content_hash = hash_content(&markdown_content);
    let revision_hash = hash_revision(RevisionHashInput {
        node_id: head.id,
        parent_id: head.parent_id,
        slug: &head.slug,
        metadata: &head.metadata,
        markdown_content: &markdown_content,
        sibling_order: head.sibling_order,
    })
    .expect("history revision hash");
    NodeRevision {
        node_id: head.id,
        parent_id: head.parent_id,
        slug: head.slug.clone(),
        metadata: head.metadata.clone(),
        markdown_content,
        sibling_order: head.sibling_order,
        version,
        content_hash,
        revision_hash,
        change_summary: Some(format!("Scale fixture revision {version}")),
        created_by: Some("scale-fixture".into()),
        created_at: version,
    }
}

fn sized_markdown(title: &str, requested_bytes: usize) -> String {
    let prefix = format!("# {title}\n\n");
    if requested_bytes <= prefix.len() {
        return prefix;
    }
    let mut content = String::with_capacity(requested_bytes);
    content.push_str(&prefix);
    content.extend(std::iter::repeat_n('x', requested_bytes - prefix.len()));
    content
}

#[cfg(test)]
mod tests {
    use super::{
        generate_large_tree_fixture, LargeTreeFixtureSpec, DEFAULT_RESPONSE_BOUNDARY_BYTES,
    };
    use crate::validate_snapshot;

    #[test]
    fn composite_scale_fixture_is_repeatable_valid_and_shaped() {
        let spec = LargeTreeFixtureSpec {
            wide_children: 12,
            deep_descendants: 9,
            history_revisions: 7,
            relations: 19,
            response_boundary_bytes: 4096,
        };
        let first = generate_large_tree_fixture(spec, 71);
        let second = generate_large_tree_fixture(spec, 71);
        assert_eq!(first, second);
        assert!(validate_snapshot(&first.snapshot).is_valid());
        assert_eq!(first.wide_child_ids.len(), 12);
        assert!(first
            .wide_child_ids
            .windows(2)
            .any(|pair| pair[0] > pair[1]));
        assert_eq!(first.snapshot.revisions.len(), 7);
        assert_eq!(first.snapshot.references.len(), 19);
        assert_eq!(first.snapshot.nodes.len(), 1 + 1 + 12 + 1 + 9 + 1 + 2);
        assert_ne!(first.deep_parent_id, first.deep_leaf_id);
        let below = first
            .snapshot
            .nodes
            .iter()
            .find(|node| node.id == first.below_boundary_node_id)
            .expect("below boundary node");
        let above = first
            .snapshot
            .nodes
            .iter()
            .find(|node| node.id == first.above_boundary_node_id)
            .expect("above boundary node");
        assert_eq!(below.markdown_content.len(), 4095);
        assert_eq!(above.markdown_content.len(), 4097);
    }

    #[test]
    fn regression_shape_crosses_collection_and_mcp_byte_boundaries() {
        let fixture = generate_large_tree_fixture(LargeTreeFixtureSpec::regression(), 72);
        assert!(fixture.wide_child_ids.len() > 100);
        let above = fixture
            .snapshot
            .nodes
            .iter()
            .find(|node| node.id == fixture.above_boundary_node_id)
            .expect("above boundary node");
        assert!(above.markdown_content.len() > DEFAULT_RESPONSE_BOUNDARY_BYTES);
    }
}
