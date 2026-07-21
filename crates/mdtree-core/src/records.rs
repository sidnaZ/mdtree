//! Derived sections, cross-references, and immutable revision records.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{DomainError, NodeHash, NodeId, NodeMetadata, Slug};

/// A Markdown section derived from canonical node content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// Stable identity of this derived section record.
    pub id: NodeId,
    /// Canonical node containing the section.
    pub node_id: NodeId,
    /// Enclosing section, if the heading is nested.
    pub parent_section_id: Option<NodeId>,
    /// Heading text, absent for content before the first heading.
    pub heading: Option<String>,
    /// Markdown heading level, absent for preamble content.
    pub heading_level: Option<u8>,
    /// Stable normalized heading anchor.
    pub anchor: Option<String>,
    /// Inclusive UTF-8 byte offset in the node Markdown.
    pub start_byte: u64,
    /// Exclusive UTF-8 byte offset in the node Markdown.
    pub end_byte: u64,
    /// Complete section content.
    pub content: String,
    /// Hash of the section content.
    pub content_hash: NodeHash,
    /// Deterministic document-order position.
    pub position: u32,
}

/// An open relationship classification such as `depends_on`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReferenceType(String);

impl ReferenceType {
    /// Returns the relationship classification as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ReferenceType {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ReferenceType {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim().is_empty() || value.contains('\0') {
            return Err(DomainError::InvalidField {
                field: "reference_type",
                reason: "must contain visible text and no NUL characters".into(),
            });
        }
        Ok(Self(value.into()))
    }
}

impl Serialize for ReferenceType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ReferenceType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Provenance of a cross-reference.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceOrigin {
    /// Explicit metadata entered by a human or mutation command.
    Explicit,
    /// Standard Markdown link extracted from content.
    Markdown,
    /// Wikilink extracted from content.
    Wikilink,
    /// Relationship read from imported metadata.
    ImportedMetadata,
    /// Non-canonical relationship inferred by diagnostics or analysis.
    Inferred,
    /// Relationship proposed or recorded by an agent.
    Agent,
}

/// Resolution state of a reference target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReferenceTarget {
    /// Target resolved uniquely to a canonical node.
    Resolved {
        /// Stable target identity.
        node_id: NodeId,
        /// Original target text retained for diagnostics and re-resolution.
        target_ref: Option<String>,
        /// Optional section anchor within the target node.
        anchor: Option<String>,
    },
    /// Original target text could not be resolved uniquely.
    Unresolved {
        /// Unmodified target text.
        target_ref: String,
    },
}

/// A typed secondary relationship that never changes canonical parentage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// Node containing or declaring the reference.
    pub source_node_id: NodeId,
    /// Source section for extracted links, if known.
    pub source_section_id: Option<NodeId>,
    /// Typed relationship classification.
    pub reference_type: ReferenceType,
    /// Resolved or unresolved target.
    pub target: ReferenceTarget,
    /// How this relationship entered the system.
    pub origin: ReferenceOrigin,
    /// Extensible origin-specific attributes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

/// Immutable snapshot of all canonical node state at one version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRevision {
    /// Stable node identity shared by every revision.
    pub node_id: NodeId,
    /// Canonical parent at this version, absent for a root snapshot.
    pub parent_id: Option<NodeId>,
    /// Canonical slug at this version.
    pub slug: Slug,
    /// Complete descriptive metadata at this version.
    pub metadata: NodeMetadata,
    /// Complete Markdown content at this version.
    pub markdown_content: String,
    /// Sibling order at this version.
    pub sibling_order: u32,
    /// Monotonically increasing node version.
    pub version: u64,
    /// Hash of Markdown content.
    pub content_hash: NodeHash,
    /// Hash of the complete canonical snapshot.
    pub revision_hash: NodeHash,
    /// Optional human-readable mutation summary.
    pub change_summary: Option<String>,
    /// Optional human or agent identity responsible for the mutation.
    pub created_by: Option<String>,
    /// Revision creation time in Unix epoch milliseconds.
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use serde::Serialize;
    use serde_json::json;

    use super::{
        NodeRevision, Reference, ReferenceOrigin, ReferenceTarget, ReferenceType, Section,
    };
    use crate::{NodeHash, NodeId, NodeMetadata, Slug};

    const NODE_ID: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
    const OTHER_ID: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XN";

    fn id(value: &str) -> NodeId {
        NodeId::from_str(value).expect("fixture ULID")
    }

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_value(value).expect("serializable record");
        serde_json::from_value(json).expect("deserializable record")
    }

    #[test]
    fn section_round_trip_preserves_every_field() {
        let section = Section {
            id: id(OTHER_ID),
            node_id: id(NODE_ID),
            parent_section_id: None,
            heading: Some("Database Models".into()),
            heading_level: Some(2),
            anchor: Some("database-models".into()),
            start_byte: 10,
            end_byte: 84,
            content: "## Database Models\nDetails.".into(),
            content_hash: NodeHash::new([3; 32]),
            position: 1,
        };

        assert_eq!(round_trip(&section), section);
    }

    #[test]
    fn references_preserve_resolution_and_distinct_origins() {
        let explicit = Reference {
            source_node_id: id(NODE_ID),
            source_section_id: None,
            reference_type: ReferenceType::from_str("depends_on").expect("reference type"),
            target: ReferenceTarget::Resolved {
                node_id: id(OTHER_ID),
                target_ref: Some("Database/Tables".into()),
                anchor: Some("products".into()),
            },
            origin: ReferenceOrigin::Explicit,
            metadata: BTreeMap::from([("weight".into(), json!(0.9))]),
        };
        let inferred = Reference {
            target: ReferenceTarget::Unresolved {
                target_ref: "Legacy inventory".into(),
            },
            origin: ReferenceOrigin::Inferred,
            ..explicit.clone()
        };

        assert_eq!(round_trip(&explicit), explicit);
        assert_eq!(round_trip(&inferred), inferred);
        assert_ne!(explicit.origin, inferred.origin);
    }

    #[test]
    fn revision_round_trip_preserves_immutable_snapshot() {
        let revision = NodeRevision {
            node_id: id(NODE_ID),
            parent_id: Some(id(OTHER_ID)),
            slug: Slug::from_str("database-models").expect("fixture slug"),
            metadata: NodeMetadata::new("Database Models"),
            markdown_content: "# Database Models".into(),
            sibling_order: 2,
            version: 4,
            content_hash: NodeHash::new([4; 32]),
            revision_hash: NodeHash::new([5; 32]),
            change_summary: Some("Clarify ownership".into()),
            created_by: Some("agent:docs".into()),
            created_at: 1_725_000_123_456,
        };

        assert_eq!(round_trip(&revision), revision);
    }
}
