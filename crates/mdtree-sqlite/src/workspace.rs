//! Workspace creation, opening, validation, and status queries.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use mdtree_core::{Node, NodeId, NodeRevision, SystemUlidGenerator};
use rusqlite::{params, Connection};
use serde::Serialize;
use tempfile::Builder;
use thiserror::Error;

use crate::store::{insert_revision, replace_derived};
use crate::{migrate, open_connection, MigrationError, WORKSPACE_FORMAT_VERSION};

/// Workspace lifecycle or validation failure.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// Destination already exists and was not overwritten.
    #[error("workspace already exists: {0}")]
    AlreadyExists(PathBuf),
    /// Supplied initial node is not a structural root.
    #[error("initial workspace node must be a root")]
    InitialNodeNotRoot,
    /// Workspace has an invalid number of roots.
    #[error("workspace must contain exactly one root; found {0}")]
    InvalidRootCount(u32),
    /// Workspace format is newer than this executable supports.
    #[error("workspace format {found} is newer than supported format {supported}")]
    UnsupportedFormat {
        /// Version stored in the workspace.
        found: u32,
        /// Latest supported format.
        supported: u32,
    },
    /// Stored root identity is malformed.
    #[error("invalid stored root ID: {0}")]
    InvalidRootId(String),
    /// Unsigned domain value cannot be represented by a `SQLite` integer.
    #[error("{field} value {value} exceeds SQLite integer range")]
    IntegerOutOfRange {
        /// Field being encoded.
        field: &'static str,
        /// Rejected unsigned value.
        value: u64,
    },
    /// A count query returned an impossible negative value.
    #[error("workspace count cannot be negative: {0}")]
    InvalidCount(i64),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Schema migration failed.
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// `SQLite` operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Canonical metadata could not be serialized.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Canonical or derived root persistence failed.
    #[error(transparent)]
    Store(#[from] crate::StoreError),
}

/// Summary of workspace storage and index state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceStatus {
    /// Workspace database path.
    pub path: PathBuf,
    /// Canonical workspace format version.
    pub format_version: u32,
    /// Installed schema migration version.
    pub schema_version: u32,
    /// Stable root node identity.
    pub root_id: NodeId,
    /// Number of canonical nodes.
    pub node_count: u64,
    /// Number of derived sections.
    pub section_count: u64,
    /// Number of cross-references.
    pub reference_count: u64,
    /// Number of currently unresolved references.
    pub unresolved_reference_count: u64,
    /// Whether the FTS table exists.
    pub search_index_present: bool,
}

/// Creates a new workspace without overwriting an existing destination.
///
/// Schema, configuration, and root initialization are completed in a temporary
/// database before it is atomically persisted at the destination.
///
/// # Errors
///
/// Returns [`WorkspaceError`] for invalid root state, an existing destination,
/// migration/storage failures, or metadata serialization failure.
pub fn create_workspace(
    path: &Path,
    name: &str,
    root: &Node,
) -> Result<Connection, WorkspaceError> {
    if !root.is_root() {
        return Err(WorkspaceError::InitialNodeNotRoot);
    }
    if path.exists() {
        return Err(WorkspaceError::AlreadyExists(path.to_path_buf()));
    }

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = Builder::new().prefix(".mdtree-").tempfile_in(directory)?;
    let mut connection = open_connection(temporary.path())?;
    migrate(&mut connection)?;

    let fields = root.fields();
    let metadata_json = serde_json::to_string(&fields.metadata)?;
    let derived = mdtree_markdown::build_derived_records(root, &SystemUlidGenerator)
        .map_err(|error| crate::StoreError::InvalidData(error.to_string()))?;
    let revision = NodeRevision {
        node_id: root.id(),
        parent_id: None,
        slug: fields.slug.clone(),
        metadata: fields.metadata.clone(),
        markdown_content: fields.markdown_content.clone(),
        sibling_order: 0,
        version: fields.version,
        content_hash: fields.content_hash,
        revision_hash: fields.revision_hash,
        change_summary: Some("Initialize workspace root".into()),
        created_by: Some("mdtree".into()),
        created_at: fields.created_at,
    };
    let created_at = sqlite_integer("created_at", fields.created_at)?;
    let updated_at = sqlite_integer("updated_at", fields.updated_at)?;
    let version = sqlite_integer("version", fields.version)?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "UPDATE workspace SET name = ?1, created_at = ?2 WHERE singleton = 1",
        params![name, created_at],
    )?;
    transaction.execute(
        "INSERT INTO nodes (
            id, parent_id, title, slug, summary, node_type, markdown_content,
            sibling_order, content_version, content_hash, revision_hash,
            metadata_json, created_at, updated_at
         ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            fields.id.to_string(),
            fields.metadata.title,
            fields.slug.as_str(),
            fields.metadata.summary,
            fields.metadata.node_type.as_ref().map(ToString::to_string),
            fields.markdown_content,
            fields.sibling_order,
            version,
            fields.content_hash.as_bytes().as_slice(),
            fields.revision_hash.as_bytes().as_slice(),
            metadata_json,
            created_at,
            updated_at,
        ],
    )?;
    insert_revision(&transaction, &revision)?;
    replace_derived(&transaction, root.id(), &derived)?;
    transaction.commit()?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(connection);

    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            WorkspaceError::AlreadyExists(path.to_path_buf())
        } else {
            WorkspaceError::Io(error.error)
        }
    })?;

    open_workspace(path)
}

/// Opens, upgrades, and validates an existing workspace.
///
/// # Errors
///
/// Returns [`WorkspaceError`] for I/O, schema, unsupported-format, or root
/// integrity failures.
pub fn open_workspace(path: &Path) -> Result<Connection, WorkspaceError> {
    let mut connection = open_connection(path)?;
    let format_version: u32 = connection.query_row(
        "SELECT format_version FROM workspace_format WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if format_version > WORKSPACE_FORMAT_VERSION {
        return Err(WorkspaceError::UnsupportedFormat {
            found: format_version,
            supported: WORKSPACE_FORMAT_VERSION,
        });
    }
    migrate(&mut connection)?;
    validate_one_root(&connection)?;
    Ok(connection)
}

/// Reads workspace versions, counts, root identity, and search-index state.
///
/// # Errors
///
/// Returns [`WorkspaceError`] if persisted state cannot be read or the root ID
/// is malformed.
pub fn workspace_status(
    connection: &Connection,
    path: &Path,
) -> Result<WorkspaceStatus, WorkspaceError> {
    let format_version = scalar(connection, "SELECT format_version FROM workspace_format")?;
    let schema_version = scalar(connection, "SELECT MAX(version) FROM schema_migrations")?;
    let root: String =
        connection.query_row("SELECT id FROM nodes WHERE parent_id IS NULL", [], |row| {
            row.get(0)
        })?;
    let root_id = NodeId::from_str(&root).map_err(|_| WorkspaceError::InvalidRootId(root))?;
    let search_index_present = scalar::<u32>(
        connection,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'section_fts'",
    )? == 1;

    Ok(WorkspaceStatus {
        path: path.to_path_buf(),
        format_version,
        schema_version,
        root_id,
        node_count: count(connection, "SELECT COUNT(*) FROM nodes")?,
        section_count: count(connection, "SELECT COUNT(*) FROM sections")?,
        reference_count: count(connection, "SELECT COUNT(*) FROM \"references\"")?,
        unresolved_reference_count: count(
            connection,
            "SELECT COUNT(*) FROM \"references\" WHERE target_node_id IS NULL",
        )?,
        search_index_present,
    })
}

fn validate_one_root(connection: &Connection) -> Result<(), WorkspaceError> {
    let count: u32 = scalar(
        connection,
        "SELECT COUNT(*) FROM nodes WHERE parent_id IS NULL",
    )?;
    if count == 1 {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidRootCount(count))
    }
}

fn scalar<T: rusqlite::types::FromSql>(connection: &Connection, sql: &str) -> rusqlite::Result<T> {
    connection.query_row(sql, [], |row| row.get(0))
}

fn count(connection: &Connection, sql: &str) -> Result<u64, WorkspaceError> {
    let value: i64 = scalar(connection, sql)?;
    u64::try_from(value).map_err(|_| WorkspaceError::InvalidCount(value))
}

fn sqlite_integer(field: &'static str, value: u64) -> Result<i64, WorkspaceError> {
    i64::try_from(value).map_err(|_| WorkspaceError::IntegerOutOfRange { field, value })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{Node, NodeFields, NodeHash, NodeId, NodeMetadata, Slug};
    use rusqlite::params;
    use tempfile::tempdir;

    use super::{create_workspace, open_workspace, workspace_status, WorkspaceError};
    use crate::{migrate, open_connection, LATEST_SCHEMA_VERSION};

    fn root() -> Node {
        Node::new(
            NodeFields {
                id: NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("fixture ID"),
                slug: Slug::from_str("project").expect("fixture slug"),
                metadata: NodeMetadata::new("Project"),
                markdown_content: "# Project".into(),
                sibling_order: 0,
                version: 1,
                content_hash: NodeHash::new([1; 32]),
                revision_hash: NodeHash::new([2; 32]),
                created_at: 100,
                updated_at: 100,
            },
            None,
        )
        .expect("fixture root")
    }

    #[test]
    fn creates_and_reports_a_complete_workspace() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("project.mdtree");
        let connection = create_workspace(&path, "Project", &root()).expect("new workspace");
        connection
            .execute(
                "INSERT INTO \"references\" (
                    source_node_id, target_ref, reference_type, origin
                 ) VALUES (?1, 'Missing', 'references', 'wikilink')",
                [root().id().to_string()],
            )
            .expect("seed unresolved reference");
        let status = workspace_status(&connection, &path).expect("workspace status");

        assert_eq!(status.root_id, root().id());
        assert_eq!(status.schema_version, LATEST_SCHEMA_VERSION);
        assert_eq!(status.node_count, 1);
        assert_eq!(status.section_count, 1);
        assert_eq!(status.reference_count, 1);
        assert_eq!(status.unresolved_reference_count, 1);
        assert!(status.search_index_present);
        assert!(matches!(
            create_workspace(&path, "Again", &root()),
            Err(WorkspaceError::AlreadyExists(_))
        ));
    }

    #[test]
    fn opening_rejects_missing_multiple_and_unsupported_roots_or_formats() {
        let directory = tempdir().expect("temporary directory");
        let missing_path = directory.path().join("missing-root.mdtree");
        let mut missing = open_connection(&missing_path).expect("connection");
        migrate(&mut missing).expect("schema");
        drop(missing);
        assert!(matches!(
            open_workspace(&missing_path),
            Err(WorkspaceError::InvalidRootCount(0))
        ));

        let multiple_path = directory.path().join("multiple-roots.mdtree");
        let connection = create_workspace(&multiple_path, "Project", &root()).expect("workspace");
        connection
            .execute_batch("DROP INDEX nodes_single_root;")
            .expect("remove protective index for corruption fixture");
        connection
            .execute(
                "INSERT INTO nodes (
                    id, title, slug, content_hash, revision_hash, created_at, updated_at
                 ) VALUES ('second-root', 'Second', 'second', ?1, ?2, 1, 1)",
                params![vec![1_u8; 32], vec![2_u8; 32]],
            )
            .expect("corrupt second root");
        drop(connection);
        assert!(matches!(
            open_workspace(&multiple_path),
            Err(WorkspaceError::InvalidRootCount(2))
        ));

        let unsupported_path = directory.path().join("unsupported.mdtree");
        let connection =
            create_workspace(&unsupported_path, "Project", &root()).expect("workspace");
        connection
            .execute("UPDATE workspace_format SET format_version = 999", [])
            .expect("future format fixture");
        drop(connection);
        assert!(matches!(
            open_workspace(&unsupported_path),
            Err(WorkspaceError::UnsupportedFormat { found: 999, .. })
        ));
    }
}
