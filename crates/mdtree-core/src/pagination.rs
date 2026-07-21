//! Shared bounded-page and opaque-cursor contracts.

use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::{NodeHash, NodeId};

const CURSOR_PREFIX: &str = "mdtree1";
const CURSOR_VERSION: u8 = 1;
const CURSOR_DOMAIN: &[u8] = b"mdtree-page-cursor-v1\0";
const SCOPE_DOMAIN: &[u8] = b"mdtree-page-scope-v1\0";

/// Smallest accepted page size.
pub const MIN_PAGE_LIMIT: u32 = 1;
/// Default page size used by adapters unless a caller supplies one.
pub const DEFAULT_PAGE_LIMIT: u32 = 50;
/// Largest accepted page size for one collection request.
pub const MAX_PAGE_LIMIT: u32 = 100;

/// Validated collection page size shared by every adapter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PageLimit(u32);

impl PageLimit {
    /// Validates an inclusive `1..=100` page size.
    ///
    /// # Errors
    ///
    /// Returns [`PaginationError::InvalidLimit`] outside the inclusive bounds.
    pub fn new(value: u32) -> Result<Self, PaginationError> {
        if !(MIN_PAGE_LIMIT..=MAX_PAGE_LIMIT).contains(&value) {
            return Err(PaginationError::InvalidLimit {
                requested: value,
                minimum: MIN_PAGE_LIMIT,
                maximum: MAX_PAGE_LIMIT,
            });
        }
        Ok(Self(value))
    }

    /// Returns the validated numeric limit.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for PageLimit {
    fn default() -> Self {
        Self(DEFAULT_PAGE_LIMIT)
    }
}

impl<'de> Deserialize<'de> for PageLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Stable continuation position for ordered collections.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PagePosition {
    /// Canonical direct-child or sibling position.
    Sibling {
        /// Primary canonical ordering field.
        sibling_order: u32,
        /// Deterministic tie-breaker.
        node_id: NodeId,
    },
    /// Canonical depth-first traversal position.
    Traversal {
        /// Stable encoded ancestry/order path used by the storage query.
        ordering_path: String,
        /// Deterministic tie-breaker.
        node_id: NodeId,
    },
    /// Canonical breadth-first traversal position.
    BreadthTraversal {
        /// Relative depth is the primary ordering field.
        depth: u32,
        /// Stable encoded ancestry/order path within the depth.
        ordering_path: String,
        /// Deterministic tie-breaker.
        node_id: NodeId,
    },
    /// Offset into a deterministically ranked search result set.
    Search {
        /// Number of ranked matches already returned.
        offset: u32,
    },
    /// Persisted reference row position.
    Reference {
        /// Monotonic canonical reference-row identity.
        row_id: u64,
    },
}

/// Binding that prevents a cursor from being reused for another request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CursorScope {
    operation: String,
    root_id: Option<NodeId>,
    request_hash: NodeHash,
}

impl CursorScope {
    /// Builds a scope from normalized operation and request keys.
    ///
    /// # Errors
    ///
    /// Returns [`PaginationError::InvalidCursorScope`] for a blank operation
    /// or a request key containing a NUL byte.
    pub fn new(
        operation: impl Into<String>,
        root_id: Option<NodeId>,
        normalized_request_key: &str,
    ) -> Result<Self, PaginationError> {
        let operation = operation.into();
        if operation.trim().is_empty() || normalized_request_key.contains('\0') {
            return Err(PaginationError::InvalidCursorScope);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(SCOPE_DOMAIN);
        hasher.update(operation.as_bytes());
        hasher.update(&[0]);
        hasher.update(normalized_request_key.as_bytes());
        Ok(Self {
            operation,
            root_id,
            request_hash: NodeHash::new(*hasher.finalize().as_bytes()),
        })
    }
}

/// Opaque, integrity-protected continuation token.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PageCursor(String);

impl PageCursor {
    /// Issues a versioned cursor bound to one workspace revision and request.
    ///
    /// # Errors
    ///
    /// Returns [`PaginationError::InvalidCursorPosition`] for an empty
    /// traversal key, or [`PaginationError::InvalidCursor`] if encoding fails.
    pub fn issue(
        workspace_revision: u64,
        scope: CursorScope,
        position: PagePosition,
    ) -> Result<Self, PaginationError> {
        if matches!(
            &position,
            PagePosition::Traversal { ordering_path, .. } if ordering_path.is_empty()
        ) {
            return Err(PaginationError::InvalidCursorPosition);
        }
        let payload = CursorPayload {
            version: CURSOR_VERSION,
            workspace_revision,
            scope,
            position,
        };
        let bytes = serde_json::to_vec(&payload).map_err(|_| PaginationError::InvalidCursor)?;
        let checksum = cursor_checksum(&bytes);
        Ok(Self(format!(
            "{CURSOR_PREFIX}.{}.{}",
            encode_hex(&bytes),
            encode_hex(checksum.as_bytes())
        )))
    }

    /// Validates integrity, request binding, and the current workspace revision.
    ///
    /// # Errors
    ///
    /// Returns a stable invalid-cursor error for malformed or mismatched
    /// tokens and [`PaginationError::StaleCursor`] after a workspace mutation.
    pub fn resume(
        &self,
        expected_scope: &CursorScope,
        current_workspace_revision: u64,
    ) -> Result<PagePosition, PaginationError> {
        let payload = decode_cursor(&self.0)?;
        if &payload.scope != expected_scope {
            return Err(PaginationError::CursorScopeMismatch);
        }
        if payload.workspace_revision != current_workspace_revision {
            return Err(PaginationError::StaleCursor {
                cursor_revision: payload.workspace_revision,
                current_revision: current_workspace_revision,
            });
        }
        Ok(payload.position)
    }

    /// Returns the adapter-facing opaque token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PageCursor {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for PageCursor {
    type Err = PaginationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_cursor(value)?;
        Ok(Self(value.into()))
    }
}

impl Serialize for PageCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PageCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Stable response envelope for every paginated collection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Page<T> {
    /// Ordered items in this page.
    pub items: Vec<T>,
    /// Opaque token for the next page, absent on the final page.
    pub next_cursor: Option<PageCursor>,
    /// Whether the collection was completely enumerated by this response.
    pub complete: bool,
    /// Whether more ordered items remain after this response.
    pub truncated: bool,
}

impl<T> Page<T> {
    /// Creates a response whose completion fields derive from continuation.
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<PageCursor>) -> Self {
        let truncated = next_cursor.is_some();
        Self {
            items,
            next_cursor,
            complete: !truncated,
            truncated,
        }
    }
}

/// Machine-readable pagination failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaginationErrorCode {
    /// Page size is outside the supported inclusive bounds.
    InvalidLimit,
    /// Cursor syntax, integrity, version, request binding, or position is invalid.
    InvalidCursor,
    /// Canonical workspace state changed after the cursor was issued.
    StaleCursor,
}

/// Stable page/cursor validation failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PaginationError {
    /// Requested page size is out of bounds.
    #[error("page limit {requested} is outside {minimum}..={maximum}")]
    InvalidLimit {
        /// Rejected value.
        requested: u32,
        /// Inclusive minimum.
        minimum: u32,
        /// Inclusive maximum.
        maximum: u32,
    },
    /// Token syntax, encoding, integrity, or version is invalid.
    #[error("invalid pagination cursor")]
    InvalidCursor,
    /// Cursor was issued for another normalized request.
    #[error("pagination cursor does not match this request")]
    CursorScopeMismatch,
    /// Scope construction was invalid.
    #[error("invalid pagination cursor scope")]
    InvalidCursorScope,
    /// Traversal position was invalid.
    #[error("invalid pagination cursor position")]
    InvalidCursorPosition,
    /// Workspace state changed after issuance.
    #[error(
        "stale pagination cursor: workspace revision changed from {cursor_revision} to {current_revision}"
    )]
    StaleCursor {
        /// Revision captured by the cursor.
        cursor_revision: u64,
        /// Current canonical workspace revision.
        current_revision: u64,
    },
}

impl PaginationError {
    /// Returns the stable adapter-facing category.
    #[must_use]
    pub const fn code(&self) -> PaginationErrorCode {
        match self {
            Self::InvalidLimit { .. } => PaginationErrorCode::InvalidLimit,
            Self::StaleCursor { .. } => PaginationErrorCode::StaleCursor,
            Self::InvalidCursor
            | Self::CursorScopeMismatch
            | Self::InvalidCursorScope
            | Self::InvalidCursorPosition => PaginationErrorCode::InvalidCursor,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CursorPayload {
    version: u8,
    workspace_revision: u64,
    scope: CursorScope,
    position: PagePosition,
}

fn decode_cursor(value: &str) -> Result<CursorPayload, PaginationError> {
    let mut parts = value.split('.');
    let prefix = parts.next().ok_or(PaginationError::InvalidCursor)?;
    let payload = parts.next().ok_or(PaginationError::InvalidCursor)?;
    let checksum = parts.next().ok_or(PaginationError::InvalidCursor)?;
    if prefix != CURSOR_PREFIX || parts.next().is_some() {
        return Err(PaginationError::InvalidCursor);
    }
    let payload = decode_hex(payload)?;
    let checksum = decode_hex(checksum)?;
    if checksum.as_slice() != cursor_checksum(&payload).as_bytes() {
        return Err(PaginationError::InvalidCursor);
    }
    let payload: CursorPayload =
        serde_json::from_slice(&payload).map_err(|_| PaginationError::InvalidCursor)?;
    if payload.version != CURSOR_VERSION {
        return Err(PaginationError::InvalidCursor);
    }
    Ok(payload)
}

fn cursor_checksum(payload: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(payload);
    hasher.finalize()
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}

fn decode_hex(value: &str) -> Result<Vec<u8>, PaginationError> {
    if !value.len().is_multiple_of(2) {
        return Err(PaginationError::InvalidCursor);
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|digits| {
            let high = hex_digit(digits[0])?;
            let low = hex_digit(digits[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, PaginationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(PaginationError::InvalidCursor),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        cursor_checksum, encode_hex, CursorPayload, CursorScope, Page, PageCursor, PageLimit,
        PagePosition, PaginationError, PaginationErrorCode, CURSOR_PREFIX, MAX_PAGE_LIMIT,
        MIN_PAGE_LIMIT,
    };
    use crate::NodeId;

    const A: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
    const B: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XN";

    fn id(value: &str) -> NodeId {
        NodeId::from_str(value).expect("node ID")
    }

    fn scope(key: &str) -> CursorScope {
        CursorScope::new("children", Some(id(A)), key).expect("scope")
    }

    #[test]
    fn limits_are_inclusive_and_reject_both_boundaries() {
        assert_eq!(PageLimit::new(MIN_PAGE_LIMIT).expect("minimum").get(), 1);
        assert_eq!(PageLimit::new(MAX_PAGE_LIMIT).expect("maximum").get(), 100);
        for value in [0, 101, u32::MAX] {
            let error = PageLimit::new(value).expect_err("invalid limit");
            assert_eq!(error.code(), PaginationErrorCode::InvalidLimit);
        }
    }

    #[test]
    fn cursors_round_trip_bind_requests_and_reject_tampering() {
        let expected_scope = scope("parent=A");
        let position = PagePosition::Sibling {
            sibling_order: 1,
            node_id: id(B),
        };
        let cursor =
            PageCursor::issue(42, expected_scope.clone(), position.clone()).expect("issued cursor");
        assert!(!cursor.as_str().contains(B));
        assert_eq!(cursor.resume(&expected_scope, 42), Ok(position));

        let json = serde_json::to_string(&cursor).expect("cursor JSON");
        assert_eq!(
            serde_json::from_str::<PageCursor>(&json).expect("cursor"),
            cursor
        );

        let mut tampered = cursor.to_string();
        let last = tampered.pop().expect("last character");
        tampered.push(if last == '0' { '1' } else { '0' });
        let error = tampered.parse::<PageCursor>().expect_err("tampered cursor");
        assert_eq!(error, PaginationError::InvalidCursor);
        assert_eq!(error.code(), PaginationErrorCode::InvalidCursor);

        assert_eq!(
            cursor.resume(&scope("parent=B"), 42),
            Err(PaginationError::CursorScopeMismatch)
        );

        let unsupported = CursorPayload {
            version: 2,
            workspace_revision: 42,
            scope: expected_scope,
            position: PagePosition::Traversal {
                ordering_path: "0000000001-B".into(),
                node_id: id(B),
            },
        };
        let payload = serde_json::to_vec(&unsupported).expect("payload");
        let token = format!(
            "{CURSOR_PREFIX}.{}.{}",
            encode_hex(&payload),
            encode_hex(cursor_checksum(&payload).as_bytes())
        );
        assert_eq!(
            token.parse::<PageCursor>(),
            Err(PaginationError::InvalidCursor)
        );
    }

    #[test]
    fn any_concurrent_canonical_mutation_makes_a_cursor_stale() {
        let expected_scope = scope("parent=A");
        let cursor = PageCursor::issue(
            7,
            expected_scope.clone(),
            PagePosition::Sibling {
                sibling_order: 0,
                node_id: id(A),
            },
        )
        .expect("cursor");
        for mutation in ["insert", "move", "reorder", "remove", "update"] {
            let error = cursor
                .resume(&expected_scope, 8)
                .expect_err("mutation must invalidate cursor");
            assert_eq!(error.code(), PaginationErrorCode::StaleCursor, "{mutation}");
            assert_eq!(
                error,
                PaginationError::StaleCursor {
                    cursor_revision: 7,
                    current_revision: 8,
                }
            );
        }
    }

    #[test]
    fn keysets_break_ties_and_page_completion_fields_cannot_drift() {
        let first = PagePosition::Sibling {
            sibling_order: 1,
            node_id: id(A),
        };
        let second = PagePosition::Sibling {
            sibling_order: 1,
            node_id: id(B),
        };
        assert!(first < second);

        let empty = Page::<NodeId>::new(Vec::new(), None);
        assert!(empty.complete);
        assert!(!empty.truncated);
        assert!(empty.next_cursor.is_none());

        let final_page = Page::new(vec![id(A), id(B)], None);
        assert!(final_page.complete);
        assert!(!final_page.truncated);

        let cursor = PageCursor::issue(1, scope("parent=A"), second).expect("cursor");
        let continued = Page::new(vec![id(A), id(B)], Some(cursor));
        assert!(!continued.complete);
        assert!(continued.truncated);
        assert!(continued.next_cursor.is_some());
    }
}
