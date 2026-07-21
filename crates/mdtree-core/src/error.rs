//! Error types shared by the domain and application layers.

use serde::Serialize;
use thiserror::Error;

/// A stable, machine-readable error category.
///
/// Variant names and their serialized `snake_case` values are part of the
/// automation-facing error contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Input failed validation.
    InvalidInput,
    /// A requested entity does not exist.
    NotFound,
    /// A selector matched more than one entity.
    Ambiguous,
    /// Current state conflicts with the requested operation.
    Conflict,
    /// A tree or workspace invariant would be violated.
    InvariantViolation,
    /// A requested response cannot fit its mandatory content into the budget.
    BudgetExceeded,
    /// The workspace or operation is not supported by this version.
    Unsupported,
    /// Persistence or another infrastructure operation failed.
    OperationFailed,
}

/// An error caused by invalid domain state or an invalid domain operation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DomainError {
    /// A field value is invalid.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// Name of the invalid field.
        field: &'static str,
        /// Human-readable validation failure.
        reason: String,
    },

    /// A requested entity was not found.
    #[error("{entity} not found: {identifier}")]
    NotFound {
        /// Kind of entity that was requested.
        entity: &'static str,
        /// User-supplied ID or selector.
        identifier: String,
    },

    /// A selector cannot be resolved uniquely.
    #[error("ambiguous {entity} selector: {selector}")]
    Ambiguous {
        /// Kind of entity being selected.
        entity: &'static str,
        /// Selector that matched multiple entities.
        selector: String,
    },

    /// Optimistic concurrency or another state precondition failed.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A canonical workspace invariant would be violated.
    #[error("invariant violation: {0}")]
    InvariantViolation(String),
}

impl DomainError {
    /// Returns the stable category for this domain error.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidField { .. } => ErrorCode::InvalidInput,
            Self::NotFound { .. } => ErrorCode::NotFound,
            Self::Ambiguous { .. } => ErrorCode::Ambiguous,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::InvariantViolation(_) => ErrorCode::InvariantViolation,
        }
    }
}

/// An error returned by an application service.
///
/// Storage adapters should convert their concrete errors into
/// [`ApplicationError::OperationFailed`] at their boundary. This keeps the core
/// independent of `SQLite`, CLI, and MCP libraries.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ApplicationError {
    /// A domain operation failed.
    #[error(transparent)]
    Domain(#[from] DomainError),

    /// Mandatory response content cannot fit into the requested budget.
    #[error("response budget exceeded: {0}")]
    BudgetExceeded(String),

    /// The executable cannot handle the requested format or operation.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// An infrastructure adapter failed to complete an operation.
    #[error("operation failed: {0}")]
    OperationFailed(String),
}

impl ApplicationError {
    /// Returns the stable category for this application error.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Domain(error) => error.code(),
            Self::BudgetExceeded(_) => ErrorCode::BudgetExceeded,
            Self::Unsupported(_) => ErrorCode::Unsupported,
            Self::OperationFailed(_) => ErrorCode::OperationFailed,
        }
    }

    /// Converts the error to the adapter-facing report contract.
    #[must_use]
    pub fn report(&self) -> ErrorReport {
        ErrorReport {
            code: self.code(),
            message: self.to_string(),
        }
    }
}

/// Stable error shape serialized by automation-facing adapters.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ErrorReport {
    /// Machine-readable error category.
    pub code: ErrorCode,
    /// Human-readable error description.
    pub message: String,
}

impl From<&ApplicationError> for ErrorReport {
    fn from(error: &ApplicationError) -> Self {
        error.report()
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplicationError, DomainError, ErrorCode, ErrorReport};

    #[test]
    fn domain_error_categories_are_stable() {
        let cases = [
            (
                DomainError::InvalidField {
                    field: "title",
                    reason: "must not be blank".into(),
                },
                ErrorCode::InvalidInput,
                "invalid title: must not be blank",
            ),
            (
                DomainError::NotFound {
                    entity: "node",
                    identifier: "missing".into(),
                },
                ErrorCode::NotFound,
                "node not found: missing",
            ),
            (
                DomainError::Ambiguous {
                    entity: "node",
                    selector: "orders".into(),
                },
                ErrorCode::Ambiguous,
                "ambiguous node selector: orders",
            ),
            (
                DomainError::Conflict("expected version 3, found 4".into()),
                ErrorCode::Conflict,
                "conflict: expected version 3, found 4",
            ),
            (
                DomainError::InvariantViolation("root cannot have a parent".into()),
                ErrorCode::InvariantViolation,
                "invariant violation: root cannot have a parent",
            ),
        ];

        for (error, code, message) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn application_error_categories_are_stable() {
        let cases = [
            (
                ApplicationError::BudgetExceeded("minimum is 128 bytes".into()),
                ErrorCode::BudgetExceeded,
                "response budget exceeded: minimum is 128 bytes",
            ),
            (
                ApplicationError::Unsupported("workspace format 2".into()),
                ErrorCode::Unsupported,
                "unsupported: workspace format 2",
            ),
            (
                ApplicationError::OperationFailed("database is locked".into()),
                ErrorCode::OperationFailed,
                "operation failed: database is locked",
            ),
        ];

        for (error, code, message) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn report_has_stable_json_shape() {
        let error = ApplicationError::from(DomainError::NotFound {
            entity: "node",
            identifier: "01ABC".into(),
        });

        let report = ErrorReport::from(&error);
        let json = serde_json::to_string(&report).expect("error report should serialize");

        assert_eq!(
            json,
            r#"{"code":"not_found","message":"node not found: 01ABC"}"#
        );
    }
}
