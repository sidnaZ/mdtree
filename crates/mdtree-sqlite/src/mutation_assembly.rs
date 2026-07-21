//! Shared canonical node, revision, and derived-record mutation assembly.

use mdtree_core::{
    hash_content, hash_revision, Node, NodeFields, NodeId, NodeMetadata, NodeRevision,
    RevisionHashInput, Slug, UlidGenerator,
};
use mdtree_markdown::{build_derived_records, DerivedNodeRecords};

use crate::{NodeChange, StoreError};

/// Adapter-independent inputs required to prepare one canonical node revision.
#[derive(Clone, Debug)]
pub struct NodeMutationDraft {
    /// Stable node identity.
    pub id: NodeId,
    /// Canonical parent, absent only for the root.
    pub parent_id: Option<NodeId>,
    /// Collision-safe canonical slug.
    pub slug: Slug,
    /// Complete replacement metadata.
    pub metadata: NodeMetadata,
    /// Complete replacement Markdown content.
    pub markdown_content: String,
    /// Deterministic sibling placement.
    pub sibling_order: u32,
    /// Resulting version, starting at one.
    pub version: u64,
    /// Original creation time in Unix milliseconds.
    pub created_at: u64,
    /// Mutation time in Unix milliseconds.
    pub updated_at: u64,
    /// Optional immutable revision author.
    pub created_by: Option<String>,
    /// Optional immutable revision reason.
    pub change_summary: Option<String>,
}

/// Fully validated mutation material ready for one store transaction.
#[derive(Clone, Debug)]
pub struct PreparedNodeMutation {
    /// Canonical replacement node with computed hashes.
    pub node: Node,
    /// Immutable revision matching the replacement node exactly.
    pub revision: NodeRevision,
    /// Precomputed sections, extracted references, and FTS documents.
    pub derived: DerivedNodeRecords,
}

impl PreparedNodeMutation {
    /// Borrows prepared material as the store's atomic change contract.
    #[must_use]
    pub const fn change(&self, expected_version: u64) -> NodeChange<'_> {
        NodeChange {
            node: &self.node,
            expected_version,
            revision: &self.revision,
            derived: &self.derived,
        }
    }
}

/// Computes hashes, validates the node, creates its revision, and parses all
/// derived records before the write transaction begins.
///
/// # Errors
///
/// Returns an error when revision hashing, node validation, or derived-record
/// construction fails.
pub fn prepare_node_mutation(
    draft: NodeMutationDraft,
    ids: &dyn UlidGenerator,
) -> Result<PreparedNodeMutation, StoreError> {
    let content_hash = hash_content(&draft.markdown_content);
    let revision_hash = hash_revision(RevisionHashInput {
        node_id: draft.id,
        parent_id: draft.parent_id,
        slug: &draft.slug,
        metadata: &draft.metadata,
        markdown_content: &draft.markdown_content,
        sibling_order: draft.sibling_order,
    })
    .map_err(|error| StoreError::InvalidData(error.to_string()))?;
    let node = Node::new(
        NodeFields {
            id: draft.id,
            slug: draft.slug,
            metadata: draft.metadata,
            markdown_content: draft.markdown_content,
            sibling_order: draft.sibling_order,
            version: draft.version,
            content_hash,
            revision_hash,
            created_at: draft.created_at,
            updated_at: draft.updated_at,
        },
        draft.parent_id,
    )
    .map_err(|error| StoreError::InvalidData(error.to_string()))?;
    let fields = node.fields();
    let revision = NodeRevision {
        node_id: node.id(),
        parent_id: node.parent_id(),
        slug: fields.slug.clone(),
        metadata: fields.metadata.clone(),
        markdown_content: fields.markdown_content.clone(),
        sibling_order: fields.sibling_order,
        version: fields.version,
        content_hash: fields.content_hash,
        revision_hash: fields.revision_hash,
        change_summary: draft.change_summary,
        created_by: draft.created_by,
        created_at: fields.updated_at,
    };
    let derived = build_derived_records(&node, ids)
        .map_err(|error| StoreError::InvalidData(error.to_string()))?;
    Ok(PreparedNodeMutation {
        node,
        revision,
        derived,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{NodeId, NodeMetadata, SequentialUlidGenerator, Slug};

    use super::{prepare_node_mutation, NodeMutationDraft};

    #[test]
    fn one_service_prepares_matching_canonical_revision_and_derived_state() {
        let draft = NodeMutationDraft {
            id: NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID"),
            parent_id: None,
            slug: Slug::from_str("project").expect("slug"),
            metadata: NodeMetadata::new("Project"),
            markdown_content: "# Project\n\n## Scope\nText.\n".into(),
            sibling_order: 0,
            version: 1,
            created_at: 10,
            updated_at: 10,
            created_by: Some("test".into()),
            change_summary: Some("Create project".into()),
        };
        let first = prepare_node_mutation(draft.clone(), &SequentialUlidGenerator::new(1))
            .expect("prepared");
        let second =
            prepare_node_mutation(draft, &SequentialUlidGenerator::new(1)).expect("prepared");
        assert_eq!(first.node, second.node);
        assert_eq!(first.revision, second.revision);
        assert_eq!(first.derived, second.derived);
        assert_eq!(
            first.revision.revision_hash,
            first.node.fields().revision_hash
        );
        assert_eq!(first.derived.content_hash, first.node.fields().content_hash);
        assert_eq!(first.derived.sections.len(), 2);
    }
}
