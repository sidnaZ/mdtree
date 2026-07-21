//! Structured, extensible node metadata.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::DomainError;

/// An open, validated node classification such as `collection` or `database_model`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeType(String);

impl NodeType {
    /// Returns the classification as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for NodeType {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for NodeType {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim().is_empty() || value.contains('\0') {
            return Err(DomainError::InvalidField {
                field: "node_type",
                reason: "must contain visible text and no NUL characters".into(),
            });
        }
        Ok(Self(value.into()))
    }
}

impl Serialize for NodeType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NodeType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Canonical descriptive metadata for a node.
///
/// Unknown JSON properties are retained in [`Self::extensions`], allowing a
/// newer snapshot to pass through an older `MDTree` version without data loss.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeMetadata {
    /// Human-readable node title.
    pub title: String,
    /// Concise description used by navigation and ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Alternate human-readable names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Optional open node classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_type: Option<NodeType>,
    /// Search and routing keywords.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Node types that may normally be placed below this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts_children: Vec<NodeType>,
    /// Concepts for which this subtree is canonical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owns: Vec<String>,
    /// Concepts that should be routed elsewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    /// User-defined labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Unrecognized metadata retained losslessly by JSON key.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl NodeMetadata {
    /// Creates metadata with a title and empty optional fields.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            summary: None,
            aliases: Vec::new(),
            node_type: None,
            keywords: Vec::new(),
            accepts_children: Vec::new(),
            owns: Vec::new(),
            excludes: Vec::new(),
            tags: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde_json::{json, Value};

    use super::{NodeMetadata, NodeType};

    #[test]
    fn known_metadata_round_trips() {
        let source = json!({
            "title": "Database Models",
            "summary": "Canonical relational entity definitions.",
            "aliases": ["Data Models", "DB Models"],
            "node_type": "collection",
            "keywords": ["schema", "entity"],
            "accepts_children": ["database_model"],
            "owns": ["database schema"],
            "excludes": ["migration procedures"],
            "tags": ["backend", "persistence"]
        });

        let metadata: NodeMetadata =
            serde_json::from_value(source.clone()).expect("valid metadata");

        assert_eq!(
            serde_json::to_value(metadata).expect("serializable metadata"),
            source
        );
    }

    #[test]
    fn unknown_metadata_round_trips_without_loss() {
        let source = json!({
            "title": "Database Models",
            "review": {"status": "approved", "reviewers": ["Ada", "Lin"]},
            "priority": 7,
            "nullable_extension": null
        });

        let metadata: NodeMetadata =
            serde_json::from_value(source.clone()).expect("valid extensible metadata");

        assert_eq!(metadata.extensions.get("priority"), Some(&Value::from(7)));
        assert_eq!(
            serde_json::to_value(metadata).expect("serializable metadata"),
            source
        );
    }

    #[test]
    fn omitted_optional_fields_use_empty_defaults() {
        let metadata: NodeMetadata =
            serde_json::from_value(json!({"title": "Root"})).expect("minimal metadata");

        assert_eq!(metadata, NodeMetadata::new("Root"));
        assert_eq!(
            serde_json::to_value(metadata).expect("serializable metadata"),
            json!({"title": "Root"})
        );
    }

    #[test]
    fn node_type_is_open_but_rejects_blank_values() {
        let custom = NodeType::from_str("team-specific/model").expect("open node type");
        assert_eq!(custom.as_str(), "team-specific/model");
        assert!(NodeType::from_str("  ").is_err());
        assert!(serde_json::from_value::<NodeType>(json!("\0invalid")).is_err());
    }
}
