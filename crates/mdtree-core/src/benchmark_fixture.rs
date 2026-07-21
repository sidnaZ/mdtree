//! Deterministic large-workspace generation for benchmarks and release checks.

use std::collections::BTreeMap;
use std::str::FromStr;

use crate::{
    hash_content, hash_revision, NodeId, NodeMetadata, NodeType, Reference, ReferenceOrigin,
    ReferenceTarget, ReferenceType, RevisionHashInput, RevisionPolicy, SequentialUlidGenerator,
    Slug, Snapshot, SnapshotNode, SnapshotWorkspace, UlidGenerator, SNAPSHOT_FORMAT_VERSION,
};

/// Generates a deterministic representative snapshot with bounded branching.
///
/// `node_count` includes the root. Each non-root node receives up to three
/// Markdown sections, typed metadata, and every tenth node links to its
/// predecessor. The fixed seed controls stable ULIDs.
///
/// # Panics
///
/// Panics only if an internally generated constant node type, relation, or
/// canonical slug is rejected, which indicates a programming error.
#[must_use]
pub fn generate_benchmark_snapshot(node_count: usize, seed: u64) -> Snapshot {
    let generator = SequentialUlidGenerator::new(seed);
    let ids = (0..node_count)
        .map(|_| NodeId::new(generator.generate()))
        .collect::<Vec<_>>();
    let mut nodes = Vec::with_capacity(node_count);
    let mut references = Vec::with_capacity(node_count / 10);
    for index in 0..node_count {
        let parent_id = (index > 0).then(|| ids[(index - 1) / 4]);
        let sibling_order = if index == 0 {
            0
        } else {
            u32::try_from((index - 1) % 4).expect("order")
        };
        let title = if index == 0 {
            "Benchmark Workspace".into()
        } else {
            format!("Node {index:05}")
        };
        let slug = if index == 0 {
            Slug::from_str("benchmark-workspace").expect("slug")
        } else {
            Slug::from_str(&format!("node-{index:05}")).expect("slug")
        };
        let mut metadata = NodeMetadata::new(title.clone());
        metadata.summary = Some(format!("Deterministic benchmark node {index}"));
        metadata.node_type = Some(
            NodeType::from_str(if index % 5 == 0 {
                "collection"
            } else {
                "document"
            })
            .expect("type"),
        );
        metadata.tags = vec![format!("group-{}", index % 16)];
        metadata.keywords = vec!["benchmark".into(), format!("topic-{}", index % 64)];
        let markdown_content = format!("# {title}\n\nRepresentative content for topic {}.\n\n## Details\n\nNode {index} uses deterministic seed {seed}.\n\n## Notes\n\nSearchable benchmark material.\n", index % 64);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: ids[index],
            parent_id,
            slug: &slug,
            metadata: &metadata,
            markdown_content: &markdown_content,
            sibling_order,
        })
        .expect("hash");
        nodes.push(SnapshotNode {
            id: ids[index],
            parent_id,
            slug,
            metadata,
            content_hash: hash_content(&markdown_content),
            revision_hash,
            markdown_content,
            sibling_order,
            version: 1,
            created_at: 1,
            updated_at: 1,
        });
        if index > 0 && index % 10 == 0 {
            references.push(Reference {
                source_node_id: ids[index],
                source_section_id: None,
                reference_type: ReferenceType::from_str("related_to").expect("relation"),
                target: ReferenceTarget::Resolved {
                    node_id: ids[index - 1],
                    target_ref: Some(format!("Node {:05}", index - 1)),
                    anchor: None,
                },
                origin: ReferenceOrigin::Agent,
                metadata: BTreeMap::new(),
            });
        }
    }
    Snapshot {
        format: "mdtree-snapshot".into(),
        format_version: SNAPSHOT_FORMAT_VERSION,
        workspace: SnapshotWorkspace {
            name: "Benchmark Workspace".into(),
            workspace_format_version: 1,
        },
        revision_policy: RevisionPolicy::HeadOnly,
        nodes,
        revisions: Vec::new(),
        references,
    }
}

#[cfg(test)]
mod tests {
    use crate::{generate_benchmark_snapshot, validate_snapshot};

    #[test]
    fn ten_thousand_nodes_are_repeatable_and_valid() {
        let first = generate_benchmark_snapshot(10_000, 42);
        let second = generate_benchmark_snapshot(10_000, 42);
        assert_eq!(first, second);
        assert!(validate_snapshot(&first).is_valid());
        assert_eq!(first.nodes.len(), 10_000);
        assert_eq!(first.references.len(), 999);
    }
}
