//! Online-safe backup, guarded restore, and non-mutating health diagnostics.

#![allow(clippy::missing_errors_doc)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tempfile::tempdir_in;
use thiserror::Error;

use crate::{SqliteStore, StoreError, WorkspaceError, LATEST_SCHEMA_VERSION};

/// Operational failure distinct from an invalid workspace report.
#[derive(Debug, Error)]
pub enum MaintenanceError {
    /// `SQLite` operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Workspace opening or compatibility failed.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// Canonical validation failed operationally.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Destination protection rejected the operation.
    #[error("destination already exists: {0}")]
    AlreadyExists(PathBuf),
    /// A backup is structurally invalid and cannot be restored.
    #[error("backup is invalid: {0:?}")]
    InvalidBackup(Vec<String>),
}

/// Stable `check` outcome suitable for process exit-code mapping.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// `SQLite` and `MDTree` invariants pass.
    Healthy,
    /// The workspace opened but failed one or more checks.
    Invalid,
}

/// Machine-readable combined `SQLite` and `MDTree` check result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckReport {
    /// Overall outcome.
    pub status: CheckStatus,
    /// `SQLite` `integrity_check` messages other than `ok`.
    pub sqlite_findings: Vec<String>,
    /// Stable `MDTree` finding codes and details.
    pub mdtree_findings: Vec<String>,
}

/// One actionable runtime diagnostic.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DoctorFinding {
    /// Stable diagnostic code.
    pub code: String,
    /// Whether the condition prevents normal operation.
    pub blocking: bool,
    /// Human-readable action or confirmation.
    pub message: String,
}

/// Non-mutating environment and workspace diagnosis.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DoctorReport {
    /// Detected `SQLite` library version.
    pub sqlite_version: String,
    /// Whether FTS5 is compiled in.
    pub fts5_available: bool,
    /// Findings including compatibility, permissions, locks, and recommendations.
    pub findings: Vec<DoctorFinding>,
}

/// Creates a consistent database backup through `SQLite`'s online backup API.
pub fn backup_workspace(store: &SqliteStore, destination: &Path) -> Result<(), MaintenanceError> {
    if destination.exists() {
        return Err(MaintenanceError::AlreadyExists(destination.to_path_buf()));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempdir_in(parent)?;
    let staged = temporary.path().join("backup.mdtree");
    let mut output = Connection::open(&staged)?;
    let backup = Backup::new(store.connection(), &mut output)?;
    backup.run_to_completion(32, Duration::from_millis(5), None)?;
    drop(backup);
    drop(output);
    validate_backup(&staged)?;
    fs::rename(staged, destination)?;
    Ok(())
}

/// Validates a backup, then atomically installs it with explicit overwrite policy.
pub fn restore_workspace(
    backup: &Path,
    destination: &Path,
    overwrite: bool,
) -> Result<(), MaintenanceError> {
    validate_backup(backup)?;
    if destination.exists() && !overwrite {
        return Err(MaintenanceError::AlreadyExists(destination.to_path_buf()));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempdir_in(parent)?;
    let staged = temporary.path().join("restore.mdtree");
    let source = Connection::open(backup)?;
    let mut output = Connection::open(&staged)?;
    let copy = Backup::new(&source, &mut output)?;
    copy.run_to_completion(32, Duration::from_millis(5), None)?;
    drop(copy);
    drop(output);
    drop(source);
    validate_backup(&staged)?;
    fs::rename(staged, destination)?;
    Ok(())
}

/// Runs `SQLite` and `MDTree` integrity checks without mutation.
pub fn check_workspace(store: &SqliteStore) -> Result<CheckReport, MaintenanceError> {
    let mut statement = store.connection().prepare("PRAGMA integrity_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let sqlite_findings = rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|finding| finding != "ok")
        .collect::<Vec<_>>();
    let mdtree_findings = store
        .validate_integrity()?
        .findings
        .into_iter()
        .map(|finding| format!("{}: {}", finding.code, finding.detail))
        .collect::<Vec<_>>();
    let status = if sqlite_findings.is_empty() && mdtree_findings.is_empty() {
        CheckStatus::Healthy
    } else {
        CheckStatus::Invalid
    };
    Ok(CheckReport {
        status,
        sqlite_findings,
        mdtree_findings,
    })
}

/// Diagnoses `SQLite` support, permissions, compatibility, locks, and repair options.
#[must_use]
pub fn doctor_workspace(path: &Path) -> DoctorReport {
    let sqlite_version = rusqlite::version().to_owned();
    let probe = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE);
    let mut findings = Vec::new();
    let mut fts5_available = false;
    match probe {
        Ok(connection) => {
            fts5_available = connection
                .query_row(
                    "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false);
            let schema = connection
                .query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap_or(0);
            findings.push(DoctorFinding {
                code: "format_compatibility".into(),
                blocking: schema > LATEST_SCHEMA_VERSION,
                message: if schema > LATEST_SCHEMA_VERSION {
                    format!("schema {schema} is newer than supported {LATEST_SCHEMA_VERSION}")
                } else {
                    format!("schema {schema} is supported")
                },
            });
            findings.push(DoctorFinding {
                code: "write_permission".into(),
                blocking: fs::metadata(path)
                    .is_ok_and(|metadata| metadata.permissions().readonly()),
                message: if fs::metadata(path)
                    .is_ok_and(|metadata| metadata.permissions().readonly())
                {
                    "workspace file is read-only".into()
                } else {
                    "workspace file is writable".into()
                },
            });
            let locked = connection
                .busy_timeout(Duration::ZERO)
                .and_then(|()| connection.execute_batch("BEGIN IMMEDIATE; ROLLBACK"))
                .is_err();
            findings.push(DoctorFinding {
                code: "write_lock".into(),
                blocking: locked,
                message: if locked {
                    "workspace has an active conflicting write lock".into()
                } else {
                    "no conflicting write lock detected".into()
                },
            });
        }
        Err(error) => findings.push(DoctorFinding {
            code: "open".into(),
            blocking: true,
            message: error.to_string(),
        }),
    }
    findings.push(DoctorFinding {
        code: "fts5".into(),
        blocking: !fts5_available,
        message: if fts5_available {
            "SQLite FTS5 is available".into()
        } else {
            "SQLite FTS5 is unavailable; install a build with FTS5".into()
        },
    });
    if let Ok(store) = SqliteStore::open(path) {
        if let Ok(report) = store.validate_integrity() {
            if !report.is_healthy() {
                findings.push(DoctorFinding {
                    code: "rebuild_recommended".into(),
                    blocking: false,
                    message: "derived integrity findings detected; run rebuild-indexes, then check"
                        .into(),
                });
            }
        }
    }
    DoctorReport {
        sqlite_version,
        fts5_available,
        findings,
    }
}

fn validate_backup(path: &Path) -> Result<(), MaintenanceError> {
    let store = SqliteStore::open(path)?;
    let report = check_workspace(&store)?;
    if report.status == CheckStatus::Healthy {
        Ok(())
    } else {
        let mut findings = report.sqlite_findings;
        findings.extend(report.mdtree_findings);
        Err(MaintenanceError::InvalidBackup(findings))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        hash_content, hash_revision, Node, NodeFields, NodeId, NodeMetadata, RevisionHashInput,
        Slug,
    };
    use tempfile::tempdir;

    use super::{
        backup_workspace, check_workspace, doctor_workspace, restore_workspace, CheckStatus,
        MaintenanceError,
    };
    use crate::{create_workspace, SqliteStore};

    fn workspace(path: &Path) -> SqliteStore {
        let id = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID");
        let slug = Slug::from_str("northstar-platform").expect("slug");
        let metadata = NodeMetadata::new("Northstar Platform");
        let markdown_content = "# Northstar Platform\n".to_owned();
        let root = Node::new(
            NodeFields {
                id,
                slug: slug.clone(),
                metadata: metadata.clone(),
                content_hash: hash_content(&markdown_content),
                revision_hash: hash_revision(RevisionHashInput {
                    node_id: id,
                    parent_id: None,
                    slug: &slug,
                    metadata: &metadata,
                    markdown_content: &markdown_content,
                    sibling_order: 0,
                })
                .expect("hash"),
                markdown_content,
                sibling_order: 0,
                version: 1,
                created_at: 1,
                updated_at: 1,
            },
            None,
        )
        .expect("root");
        SqliteStore::new(create_workspace(path, "Northstar Platform", &root).expect("workspace"))
    }

    use std::path::Path;

    #[test]
    fn online_backup_guarded_restore_check_and_doctor_are_safe() {
        let directory = tempdir().expect("tempdir");
        let source = directory.path().join("source.mdtree");
        let store = workspace(&source);
        let backup = directory.path().join("backup.mdtree");
        backup_workspace(&store, &backup).expect("backup");
        assert!(matches!(
            backup_workspace(&store, &backup),
            Err(MaintenanceError::AlreadyExists(_))
        ));
        let restored = directory.path().join("restored.mdtree");
        restore_workspace(&backup, &restored, false).expect("restore");
        let restored_store = SqliteStore::open(&restored).expect("restored workspace");
        assert_eq!(
            check_workspace(&restored_store).expect("check").status,
            CheckStatus::Healthy
        );
        assert!(doctor_workspace(&restored).fts5_available);
        assert!(matches!(
            restore_workspace(&backup, &restored, false),
            Err(MaintenanceError::AlreadyExists(_))
        ));

        let invalid = directory.path().join("invalid.mdtree");
        std::fs::write(&invalid, b"not sqlite").expect("invalid backup");
        let original = std::fs::read(&restored).expect("original bytes");
        assert!(restore_workspace(&invalid, &restored, true).is_err());
        assert_eq!(std::fs::read(&restored).expect("preserved"), original);

        restored_store
            .connection()
            .execute("DELETE FROM sections", [])
            .expect("corrupt derived rows");
        let invalid_report = check_workspace(&restored_store).expect("invalid report");
        assert_eq!(invalid_report.status, CheckStatus::Invalid);
        assert!(!invalid_report.mdtree_findings.is_empty());
        assert!(doctor_workspace(&restored)
            .findings
            .iter()
            .any(|finding| finding.code == "rebuild_recommended"));
    }
}
