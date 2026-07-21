//! Stable node identity and human-facing locator value types.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

use crate::DomainError;

/// Stable node identity represented by a ULID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(Ulid);

impl NodeId {
    /// Wraps an already validated ULID.
    #[must_use]
    pub const fn new(value: Ulid) -> Self {
        Self(value)
    }

    /// Returns the underlying ULID.
    #[must_use]
    pub const fn into_inner(self) -> Ulid {
        self.0
    }
}

impl Display for NodeId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for NodeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl FromStr for NodeId {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ulid::from_str(value)
            .map(Self)
            .map_err(|_| DomainError::InvalidField {
                field: "node_id",
                reason: "must be a valid ULID".into(),
            })
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Validated canonical path segment.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Slug(String);

impl Slug {
    pub(crate) fn from_normalized(value: String) -> Self {
        debug_assert!(value.parse::<Self>().is_ok());
        Self(value)
    }

    /// Returns the slug as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Slug {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Slug {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = !value.is_empty()
            && !value.starts_with('-')
            && !value.ends_with('-')
            && !value.contains("--")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');

        if valid {
            Ok(Self(value.into()))
        } else {
            Err(DomainError::InvalidField {
                field: "slug",
                reason: "must contain lowercase ASCII letters, digits, or single interior hyphens"
                    .into(),
            })
        }
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Root-to-node titles suitable for display in search and navigation results.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Breadcrumb(Vec<String>);

impl Breadcrumb {
    /// Builds a non-empty breadcrumb whose segments contain visible text.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidField`] when there are no segments or a
    /// segment is blank or contains a NUL character.
    pub fn new(segments: Vec<String>) -> Result<Self, DomainError> {
        if segments.is_empty() {
            return Err(invalid_breadcrumb("must contain at least one segment"));
        }
        if segments.iter().any(|segment| segment.trim().is_empty()) {
            return Err(invalid_breadcrumb("segments must not be blank"));
        }
        if segments.iter().any(|segment| segment.contains('\0')) {
            return Err(invalid_breadcrumb("segments must not contain NUL"));
        }
        Ok(Self(segments))
    }

    /// Returns the root-to-node title segments.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

impl Display for Breadcrumb {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0.join(" > "))
    }
}

impl<'de> Deserialize<'de> for Breadcrumb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let segments = Vec::<String>::deserialize(deserializer)?;
        Self::new(segments).map_err(serde::de::Error::custom)
    }
}

/// A user-supplied node locator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeSelector {
    /// Stable ULID selector.
    Id(NodeId),
    /// Canonical root-relative path of slugs.
    Path(Vec<Slug>),
    /// A single slug, which may require ambiguity resolution.
    Slug(Slug),
}

impl FromStr for NodeSelector {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(DomainError::InvalidField {
                field: "node_selector",
                reason: "must not be blank".into(),
            });
        }

        if let Some(id) = value.strip_prefix("id:") {
            return id.parse().map(Self::Id);
        }

        if let Ok(id) = value.parse() {
            return Ok(Self::Id(id));
        }

        let path = value.strip_prefix("path:").unwrap_or(value);
        if path.contains('/') {
            let path = path.trim_matches('/');
            if path.is_empty() {
                return Err(invalid_selector("path must contain at least one slug"));
            }
            return path
                .split('/')
                .map(str::parse)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Path);
        }

        path.parse().map(Self::Slug)
    }
}

fn invalid_breadcrumb(reason: &str) -> DomainError {
    DomainError::InvalidField {
        field: "breadcrumb",
        reason: reason.into(),
    }
}

fn invalid_selector(reason: &str) -> DomainError {
    DomainError::InvalidField {
        field: "node_selector",
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{Breadcrumb, NodeId, NodeSelector, Slug};

    const ID: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";

    #[test]
    fn node_id_accepts_and_round_trips_a_ulid() {
        let id = NodeId::from_str(ID).expect("valid ULID");
        assert_eq!(id.to_string(), ID);
        let decoded = serde_json::from_str::<NodeId>(&format!(r#""{ID}""#))
            .expect("serialized node ID should be valid");
        assert_eq!(decoded, id);
    }

    #[test]
    fn node_id_rejects_malformed_values() {
        for value in ["", "not-a-ulid", "01JZ8Q5CWPN8T7KPN5A1V9B6X!"] {
            assert!(NodeId::from_str(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn slug_accepts_canonical_values() {
        for value in ["root", "database-models", "api-v2"] {
            assert_eq!(Slug::from_str(value).expect("valid slug").as_str(), value);
        }
    }

    #[test]
    fn slug_rejects_noncanonical_values() {
        for value in [
            "",
            "Database",
            "two words",
            "-edge",
            "edge-",
            "two--hyphens",
            "dati-ā",
        ] {
            assert!(Slug::from_str(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn breadcrumb_validates_and_formats_segments() {
        let breadcrumb = Breadcrumb::new(vec!["Project".into(), "Database Models".into()])
            .expect("valid breadcrumb");
        assert_eq!(breadcrumb.to_string(), "Project > Database Models");
        assert!(Breadcrumb::new(Vec::new()).is_err());
        assert!(Breadcrumb::new(vec!["Project".into(), "  ".into()]).is_err());
    }

    #[test]
    fn selector_parses_ids_slugs_and_paths() {
        assert!(matches!(
            NodeSelector::from_str(ID),
            Ok(NodeSelector::Id(_))
        ));
        assert_eq!(
            NodeSelector::from_str("architecture/backend/database-models"),
            Ok(NodeSelector::Path(vec![
                Slug::from_str("architecture").expect("valid slug"),
                Slug::from_str("backend").expect("valid slug"),
                Slug::from_str("database-models").expect("valid slug"),
            ]))
        );
        assert_eq!(
            NodeSelector::from_str("database-models"),
            Ok(NodeSelector::Slug(
                Slug::from_str("database-models").expect("valid slug")
            ))
        );
    }

    #[test]
    fn selector_rejects_blank_and_malformed_paths() {
        for value in ["", " ", "/", "architecture//backend", "path:/"] {
            assert!(NodeSelector::from_str(value).is_err(), "accepted {value:?}");
        }
    }
}
