//! Canonical node state and its construction invariants.

use serde::{Deserialize, Serialize};

use crate::{DomainError, NodeId, NodeMetadata, Slug};

/// A 256-bit digest used for canonical content and complete revision state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NodeHash([u8; 32]);

impl NodeHash {
    /// Wraps raw digest bytes produced by the canonical hashing service.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Non-structural fields supplied when constructing a canonical node.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeFields {
    /// Stable node identity.
    pub id: NodeId,
    /// Canonical path segment under the node's parent.
    pub slug: Slug,
    /// Descriptive metadata, including the required title.
    pub metadata: NodeMetadata,
    /// Canonical Markdown content.
    pub markdown_content: String,
    /// Zero-based order among siblings.
    pub sibling_order: u32,
    /// Optimistic-concurrency version, starting at one.
    pub version: u64,
    /// Hash of canonical Markdown content.
    pub content_hash: NodeHash,
    /// Hash of all canonical state represented by this revision.
    pub revision_hash: NodeHash,
    /// Creation time in Unix epoch milliseconds.
    pub created_at: u64,
    /// Last canonical mutation time in Unix epoch milliseconds.
    pub updated_at: u64,
}

/// One canonical Markdown node in the strict workspace tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    fields: NodeFields,
    parent_id: Option<NodeId>,
}

impl Node {
    /// Constructs validated canonical node state.
    ///
    /// A missing parent creates a root and therefore requires sibling order
    /// zero. A non-root node must not identify itself as its parent.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidField`] for a blank title, version zero,
    /// or timestamps in reverse order. Returns
    /// [`DomainError::InvariantViolation`] for an invalid root/non-root
    /// structural combination.
    pub fn new(fields: NodeFields, parent_id: Option<NodeId>) -> Result<Self, DomainError> {
        validate_title(&fields.metadata.title)?;
        if fields.version == 0 {
            return Err(DomainError::InvalidField {
                field: "version",
                reason: "must be at least 1".into(),
            });
        }
        if fields.updated_at < fields.created_at {
            return Err(DomainError::InvalidField {
                field: "updated_at",
                reason: "must not be earlier than created_at".into(),
            });
        }

        match parent_id {
            None if fields.sibling_order != 0 => {
                return Err(DomainError::InvariantViolation(
                    "root sibling order must be 0".into(),
                ));
            }
            Some(parent) if parent == fields.id => {
                return Err(DomainError::InvariantViolation(
                    "a non-root node cannot be its own parent".into(),
                ));
            }
            _ => {}
        }

        Ok(Self { fields, parent_id })
    }

    /// Returns the stable identity.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.fields.id
    }

    /// Returns the canonical parent, or `None` for the root.
    #[must_use]
    pub const fn parent_id(&self) -> Option<NodeId> {
        self.parent_id
    }

    /// Returns whether this node is the structural root.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// Returns all non-parent canonical fields.
    #[must_use]
    pub const fn fields(&self) -> &NodeFields {
        &self.fields
    }
}

fn validate_title(title: &str) -> Result<(), DomainError> {
    if title.trim().is_empty() {
        return Err(DomainError::InvalidField {
            field: "title",
            reason: "must not be blank".into(),
        });
    }
    if title.contains('\0') {
        return Err(DomainError::InvalidField {
            field: "title",
            reason: "must not contain NUL".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{Node, NodeFields, NodeHash};
    use crate::{NodeId, NodeMetadata, Slug};

    fn fields(id: &str) -> NodeFields {
        NodeFields {
            id: NodeId::from_str(id).expect("fixture node ID"),
            slug: Slug::from_str("root").expect("fixture slug"),
            metadata: NodeMetadata::new("Project"),
            markdown_content: "# Project".into(),
            sibling_order: 0,
            version: 1,
            content_hash: NodeHash::new([1; 32]),
            revision_hash: NodeHash::new([2; 32]),
            created_at: 1_725_000_000_000,
            updated_at: 1_725_000_000_000,
        }
    }

    #[test]
    fn constructs_valid_root_and_non_root_nodes() {
        let root_fields = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
        let root_id = root_fields.id;
        let root = Node::new(root_fields, None).expect("valid root");
        assert!(root.is_root());

        let mut child_fields = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XN");
        child_fields.slug = Slug::from_str("child").expect("fixture slug");
        child_fields.sibling_order = 3;
        let child = Node::new(child_fields, Some(root_id)).expect("valid child");
        assert_eq!(child.parent_id(), Some(root_id));
    }

    #[test]
    fn rejects_invalid_titles() {
        for title in ["", "   ", "Project\0Hidden"] {
            let mut candidate = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
            candidate.metadata.title = title.into();
            assert!(Node::new(candidate, None).is_err(), "accepted {title:?}");
        }
    }

    #[test]
    fn rejects_invalid_versions_and_timestamps() {
        let mut zero_version = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
        zero_version.version = 0;
        assert!(Node::new(zero_version, None).is_err());

        let mut reversed_time = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
        reversed_time.updated_at = reversed_time.created_at - 1;
        assert!(Node::new(reversed_time, None).is_err());
    }

    #[test]
    fn rejects_invalid_root_and_non_root_combinations() {
        let mut ordered_root = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
        ordered_root.sibling_order = 1;
        assert!(Node::new(ordered_root, None).is_err());

        let self_parent = fields("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
        let id = self_parent.id;
        assert!(Node::new(self_parent, Some(id)).is_err());
    }
}
