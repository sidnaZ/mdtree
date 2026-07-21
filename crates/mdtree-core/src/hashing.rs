//! Canonical BLAKE3 content and revision hashing.

use serde_json::Value;

use crate::{ApplicationError, NodeHash, NodeId, NodeMetadata, Slug};

const CONTENT_DOMAIN: &[u8] = b"mdtree-content-v1\0";
const REVISION_DOMAIN: &[u8] = b"mdtree-revision-v1\0";

/// Canonical state included in a revision hash.
///
/// Identity, parentage, slug, sibling order, metadata, and exact Markdown bytes
/// are included. Version numbers, timestamps, authors, and change summaries are
/// intentionally excluded because they are audit data rather than semantic
/// canonical state.
#[derive(Clone, Copy, Debug)]
pub struct RevisionHashInput<'a> {
    /// Stable node identity.
    pub node_id: NodeId,
    /// Canonical parent, absent for the root.
    pub parent_id: Option<NodeId>,
    /// Canonical path segment.
    pub slug: &'a Slug,
    /// Complete descriptive metadata.
    pub metadata: &'a NodeMetadata,
    /// Exact canonical Markdown.
    pub markdown_content: &'a str,
    /// Canonical sibling order.
    pub sibling_order: u32,
}

/// Hashes the exact UTF-8 bytes of canonical Markdown with a versioned domain separator.
#[must_use]
pub fn hash_content(markdown: &str) -> NodeHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CONTENT_DOMAIN);
    frame(&mut hasher, markdown.as_bytes());
    NodeHash::new(*hasher.finalize().as_bytes())
}

/// Hashes every documented semantic field using deterministic framing.
///
/// Metadata object keys are recursively sorted before serialization, so map
/// insertion order cannot change the digest.
///
/// # Errors
///
/// Returns [`ApplicationError::OperationFailed`] if metadata cannot be
/// converted to canonical JSON.
pub fn hash_revision(input: RevisionHashInput<'_>) -> Result<NodeHash, ApplicationError> {
    let metadata = serde_json::to_value(input.metadata)
        .and_then(|value| serde_json::to_vec(&canonicalize(value)))
        .map_err(|error| ApplicationError::OperationFailed(format!("hash metadata: {error}")))?;
    let content_hash = hash_content(input.markdown_content);

    let mut hasher = blake3::Hasher::new();
    hasher.update(REVISION_DOMAIN);
    frame(&mut hasher, input.node_id.to_string().as_bytes());
    match input.parent_id {
        Some(parent_id) => {
            hasher.update(&[1]);
            frame(&mut hasher, parent_id.to_string().as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    frame(&mut hasher, input.slug.as_str().as_bytes());
    frame(&mut hasher, &input.sibling_order.to_be_bytes());
    frame(&mut hasher, &metadata);
    frame(&mut hasher, content_hash.as_bytes());

    Ok(NodeHash::new(*hasher.finalize().as_bytes()))
}

fn frame(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde_json::json;

    use super::{hash_content, hash_revision, RevisionHashInput};
    use crate::{NodeId, NodeMetadata, Slug};

    const NODE_ID: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
    const PARENT_ID: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XN";

    fn fixture<'a>(
        slug: &'a Slug,
        metadata: &'a NodeMetadata,
        markdown_content: &'a str,
    ) -> RevisionHashInput<'a> {
        RevisionHashInput {
            node_id: NodeId::from_str(NODE_ID).expect("fixture ID"),
            parent_id: Some(NodeId::from_str(PARENT_ID).expect("fixture parent ID")),
            slug,
            metadata,
            markdown_content,
            sibling_order: 3,
        }
    }

    #[test]
    fn content_hash_is_deterministic_and_byte_exact() {
        assert_eq!(hash_content("# Node\n"), hash_content("# Node\n"));
        assert_ne!(hash_content("# Node\n"), hash_content("# Node"));
    }

    #[test]
    fn revision_hash_is_deterministic_and_metadata_order_independent() {
        let slug = Slug::from_str("database-models").expect("fixture slug");
        let mut first = NodeMetadata::new("Database Models");
        first.extensions.insert("zeta".into(), json!(1));
        first
            .extensions
            .insert("alpha".into(), json!({"b": 2, "a": 1}));
        let mut second = NodeMetadata::new("Database Models");
        second
            .extensions
            .insert("alpha".into(), json!({"a": 1, "b": 2}));
        second.extensions.insert("zeta".into(), json!(1));

        assert_eq!(
            hash_revision(fixture(&slug, &first, "# Models")).expect("hashable state"),
            hash_revision(fixture(&slug, &second, "# Models")).expect("hashable state")
        );
    }

    #[test]
    fn every_documented_semantic_input_changes_revision_hash() {
        let slug = Slug::from_str("database-models").expect("fixture slug");
        let metadata = NodeMetadata::new("Database Models");
        let original = fixture(&slug, &metadata, "# Models");
        let baseline = hash_revision(original).expect("hashable state");

        let other_slug = Slug::from_str("data-models").expect("fixture slug");
        let changed_metadata = NodeMetadata::new("Data Models");
        let changes = [
            RevisionHashInput {
                node_id: NodeId::from_str(PARENT_ID).expect("fixture ID"),
                ..original
            },
            RevisionHashInput {
                parent_id: None,
                ..original
            },
            RevisionHashInput {
                slug: &other_slug,
                ..original
            },
            RevisionHashInput {
                metadata: &changed_metadata,
                ..original
            },
            RevisionHashInput {
                markdown_content: "# Models\nChanged",
                ..original
            },
            RevisionHashInput {
                sibling_order: 4,
                ..original
            },
        ];

        for changed in changes {
            assert_ne!(hash_revision(changed).expect("hashable state"), baseline);
        }
    }
}
