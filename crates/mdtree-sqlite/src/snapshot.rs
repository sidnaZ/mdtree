//! Deterministic JSON snapshot export, planning, and transactional import.

#![allow(clippy::missing_errors_doc)]

use std::path::Path;
use std::str::FromStr;

use mdtree_core::{
    validate_snapshot, Node, NodeFields, NodeId, NodeRevision, RevisionPolicy, Snapshot,
    SnapshotNode, SnapshotValidationReport, SnapshotWorkspace, SystemUlidGenerator,
    SNAPSHOT_FORMAT_VERSION,
};
use tempfile::tempdir_in;
use thiserror::Error;

use crate::store::{insert_node, insert_reference, insert_revision, replace_derived};
use crate::{create_workspace, SqliteStore, StoreError, WorkspaceError, WORKSPACE_FORMAT_VERSION};

/// Parsed snapshot plus its complete pre-mutation validation report.
#[derive(Clone, Debug)]
pub struct ImportPlan {
    /// Parsed candidate snapshot.
    pub snapshot: Snapshot,
    /// Aggregate actionable validation errors.
    pub validation: SnapshotValidationReport,
}

/// Snapshot serialization, validation, or import failure.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// JSON is syntactically or structurally invalid.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Markdown snapshot layout or YAML is invalid.
    #[error(transparent)]
    Markdown(#[from] mdtree_markdown::MarkdownSnapshotError),
    /// Candidate failed complete validation.
    #[error("snapshot validation failed: {0:?}")]
    Invalid(SnapshotValidationReport),
    /// Workspace lifecycle failed.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// Canonical storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Exports complete canonical heads, history, and typed references in deterministic order.
///
/// This is an interchange/backup primitive. Targeted reads must use store
/// projections so they do not load unrelated nodes, history, or references.
pub fn export_snapshot(store: &SqliteStore) -> Result<Snapshot, SnapshotError> {
    let workspace_name: String = store
        .connection()
        .query_row("SELECT name FROM workspace WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .map_err(StoreError::from)?;
    let mut statement = store
        .connection()
        .prepare("SELECT id FROM nodes ORDER BY id")
        .map_err(StoreError::from)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(StoreError::from)?
        .map(|row| {
            NodeId::from_str(&row.map_err(StoreError::from)?)
                .map_err(|e| StoreError::InvalidData(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut nodes = Vec::new();
    let mut revisions = Vec::new();
    let mut references = Vec::new();
    for id in ids {
        let node = store
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        nodes.push(exported_node(&node));
        revisions.extend(store.revisions(id)?);
        references.extend(store.outgoing_references(id)?);
    }
    revisions.sort_by_key(|revision| (revision.node_id, revision.version));
    references.sort_by_key(|reference| serde_json::to_string(reference).unwrap_or_default());
    Ok(Snapshot {
        format: "mdtree-snapshot".into(),
        format_version: SNAPSHOT_FORMAT_VERSION,
        workspace: SnapshotWorkspace {
            name: workspace_name,
            workspace_format_version: WORKSPACE_FORMAT_VERSION,
        },
        revision_policy: RevisionPolicy::Complete,
        nodes,
        revisions,
        references,
    })
}

/// Serializes a byte-stable pretty JSON snapshot of the complete workspace.
///
/// This is an interchange/backup primitive, not a targeted query mechanism.
pub fn export_snapshot_json(store: &SqliteStore) -> Result<Vec<u8>, SnapshotError> {
    let mut bytes = serde_json::to_vec_pretty(&export_snapshot(store)?)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Parses and fully validates JSON without mutating a workspace.
pub fn plan_json_import(bytes: &[u8]) -> Result<ImportPlan, SnapshotError> {
    let snapshot: Snapshot = serde_json::from_slice(bytes)?;
    let validation = validate_snapshot(&snapshot);
    Ok(ImportPlan {
        snapshot,
        validation,
    })
}

/// Exports a complete workspace to a deterministic Markdown directory tree.
///
/// Targeted Markdown export uses [`export_markdown_node`] instead.
pub fn export_markdown_snapshot(store: &SqliteStore, path: &Path) -> Result<(), SnapshotError> {
    mdtree_markdown::export_markdown_snapshot(path, &export_snapshot(store)?)?;
    Ok(())
}

/// Exports one canonical node and descendants up to an optional relative depth.
///
/// `max_depth` uses the selected node as depth zero. `None` exports the complete subtree.
pub fn export_markdown_node(
    store: &SqliteStore,
    id: NodeId,
    path: &Path,
    max_depth: Option<u32>,
) -> Result<Vec<std::path::PathBuf>, SnapshotError> {
    let nodes = store
        .subtree(id)?
        .into_iter()
        .filter(|item| max_depth.is_none_or(|depth| item.depth <= depth))
        .map(|item| exported_node(&item.node))
        .collect::<Vec<_>>();
    Ok(mdtree_markdown::export_markdown_subtree(path, &nodes)?)
}

fn exported_node(node: &Node) -> SnapshotNode {
    SnapshotNode::from(node)
}

/// Parses and fully validates a Markdown snapshot without mutating a workspace.
pub fn plan_markdown_import(path: &Path) -> Result<ImportPlan, SnapshotError> {
    let snapshot = mdtree_markdown::parse_markdown_snapshot(path)?;
    let validation = validate_snapshot(&snapshot);
    Ok(ImportPlan {
        snapshot,
        validation,
    })
}

/// Imports a Markdown snapshot into a new workspace after complete planning.
pub fn import_markdown_snapshot_new(
    snapshot_path: &Path,
    workspace_path: &Path,
) -> Result<(), SnapshotError> {
    let plan = plan_markdown_import(snapshot_path)?;
    if !plan.validation.is_valid() {
        return Err(SnapshotError::Invalid(plan.validation));
    }
    import_snapshot_new(workspace_path, &plan.snapshot)
}

/// Imports a validated snapshot into a new workspace; existing destinations are never merged.
pub fn import_snapshot_new(path: &Path, snapshot: &Snapshot) -> Result<(), SnapshotError> {
    let validation = validate_snapshot(snapshot);
    if !validation.is_valid() {
        return Err(SnapshotError::Invalid(validation));
    }
    if path.exists() {
        return Err(SnapshotError::Workspace(WorkspaceError::AlreadyExists(
            path.to_path_buf(),
        )));
    }
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempdir_in(directory)?;
    let temporary_path = temporary.path().join("import.mdtree");
    let root_snapshot = snapshot
        .nodes
        .iter()
        .find(|node| node.parent_id.is_none())
        .ok_or_else(|| SnapshotError::Invalid(validation.clone()))?;
    let root = snapshot_node(root_snapshot)?;
    let connection = create_workspace(&temporary_path, &snapshot.workspace.name, &root)?;
    let mut store = SqliteStore::new(connection);
    let mut canonical = Vec::new();
    let mut derived = Vec::new();
    for snapshot_node_value in &snapshot.nodes {
        let node = snapshot_node(snapshot_node_value)?;
        let records = mdtree_markdown::build_derived_records(&node, &SystemUlidGenerator)
            .map_err(|e| StoreError::InvalidData(e.to_string()))?;
        canonical.push(node);
        derived.push(records);
    }
    let transaction = store
        .connection_mut()
        .transaction()
        .map_err(StoreError::from)?;
    for node in canonical.iter().filter(|node| !node.is_root()) {
        insert_node(&transaction, node)?;
    }
    transaction
        .execute("DELETE FROM node_versions", [])
        .map_err(StoreError::from)?;
    if snapshot.revisions.is_empty() {
        for node in &canonical {
            insert_revision(&transaction, &head_revision(node))?;
        }
    } else {
        for revision in &snapshot.revisions {
            insert_revision(&transaction, revision)?;
        }
    }
    transaction
        .execute("DELETE FROM section_fts", [])
        .map_err(StoreError::from)?;
    transaction
        .execute("DELETE FROM \"references\"", [])
        .map_err(StoreError::from)?;
    transaction
        .execute("DELETE FROM sections", [])
        .map_err(StoreError::from)?;
    for (node, records) in canonical.iter().zip(&derived) {
        replace_derived(&transaction, node.id(), records)?;
    }
    for reference in &snapshot.references {
        insert_reference(&transaction, reference)?;
    }
    transaction.commit().map_err(StoreError::from)?;
    store
        .connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(StoreError::from)?;
    drop(store);
    std::fs::rename(&temporary_path, path)?;
    Ok(())
}

fn snapshot_node(value: &SnapshotNode) -> Result<Node, SnapshotError> {
    Node::new(
        NodeFields {
            id: value.id,
            slug: value.slug.clone(),
            metadata: value.metadata.clone(),
            markdown_content: value.markdown_content.clone(),
            sibling_order: value.sibling_order,
            version: value.version,
            content_hash: value.content_hash,
            revision_hash: value.revision_hash,
            created_at: value.created_at,
            updated_at: value.updated_at,
        },
        value.parent_id,
    )
    .map_err(|error| SnapshotError::Store(StoreError::InvalidData(error.to_string())))
}
fn head_revision(node: &Node) -> NodeRevision {
    let f = node.fields();
    NodeRevision {
        node_id: node.id(),
        parent_id: node.parent_id(),
        slug: f.slug.clone(),
        metadata: f.metadata.clone(),
        markdown_content: f.markdown_content.clone(),
        sibling_order: f.sibling_order,
        version: f.version,
        content_hash: f.content_hash,
        revision_hash: f.revision_hash,
        change_summary: Some("Imported snapshot head".into()),
        created_by: Some("import".into()),
        created_at: f.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        export_markdown_snapshot, export_snapshot, export_snapshot_json, head_revision,
        import_markdown_snapshot_new, import_snapshot_new, plan_json_import, plan_markdown_import,
    };
    use crate::{create_workspace, SqliteStore};
    use mdtree_core::{
        hash_content, hash_revision, Node, NodeFields, NodeId, NodeMetadata, NodeRevision,
        RevisionHashInput, SequentialUlidGenerator, Slug,
    };
    use std::str::FromStr;
    use tempfile::tempdir;

    fn node(raw: &str, parent: Option<NodeId>, title: &str, slug: &str) -> Node {
        let id = NodeId::from_str(raw).expect("ID");
        let slug = Slug::from_str(slug).expect("slug");
        let metadata = NodeMetadata::new(title);
        let content = format!("# {title}\n");
        let content_hash = hash_content(&content);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: id,
            parent_id: parent,
            slug: &slug,
            metadata: &metadata,
            markdown_content: &content,
            sibling_order: 0,
        })
        .expect("hash");
        Node::new(
            NodeFields {
                id,
                slug,
                metadata,
                markdown_content: content,
                sibling_order: 0,
                version: 1,
                content_hash,
                revision_hash,
                created_at: 1,
                updated_at: 1,
            },
            parent,
        )
        .expect("node")
    }
    fn revision(node: &Node) -> NodeRevision {
        head_revision(node)
    }

    #[test]
    fn json_export_is_stable_validation_is_complete_and_import_round_trips() {
        let directory = tempdir().expect("tempdir");
        let source_path = directory.path().join("source.mdtree");
        let root = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XM",
            None,
            "Northstar Platform",
            "northstar-platform",
        );
        let connection =
            create_workspace(&source_path, "Northstar Platform", &root).expect("workspace");
        let mut store = SqliteStore::new(connection);
        let child = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Orders",
            "orders",
        );
        let derived =
            mdtree_markdown::build_derived_records(&child, &SequentialUlidGenerator::new(100))
                .expect("derived");
        store
            .create_node(&child, &revision(&child), &derived)
            .expect("create");
        store.connection().execute("INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin) VALUES (?1,?2,'Project','depends_on','explicit')",rusqlite::params![child.id().to_string(),root.id().to_string()]).expect("reference");
        let first = export_snapshot_json(&store).expect("export");
        let second = export_snapshot_json(&store).expect("export");
        assert_eq!(first, second);
        let plan = plan_json_import(&first).expect("plan");
        assert!(plan.validation.is_valid());
        let mut invalid = plan.snapshot.clone();
        invalid.nodes.push(invalid.nodes[1].clone());
        invalid.nodes[0].metadata.title = " ".into();
        let invalid_plan =
            plan_json_import(&serde_json::to_vec(&invalid).expect("JSON")).expect("plan");
        let codes: Vec<_> = invalid_plan
            .validation
            .errors
            .iter()
            .map(|error| error.code.as_str())
            .collect();
        assert!(codes.contains(&"duplicate_id"));
        assert!(codes.contains(&"title"));
        let before: u32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(before, 2);
        let imported_path = directory.path().join("imported.mdtree");
        import_snapshot_new(&imported_path, &plan.snapshot).expect("import");
        let imported = SqliteStore::open(&imported_path).expect("open import");
        assert_eq!(
            export_snapshot(&imported).expect("export imported"),
            plan.snapshot
        );
        assert_eq!(
            imported
                .get(child.id())
                .expect("read")
                .expect("child")
                .fields()
                .metadata
                .title,
            "Orders"
        );
        assert_eq!(imported.backlinks(root.id()).expect("backlinks").len(), 1);

        let markdown_path = directory.path().join("markdown");
        export_markdown_snapshot(&store, &markdown_path).expect("Markdown export");
        let first_manifest = std::fs::read(markdown_path.join("workspace.yaml")).expect("manifest");
        let planned = plan_markdown_import(&markdown_path).expect("Markdown plan");
        assert!(planned.validation.is_valid());
        assert_eq!(planned.snapshot, plan.snapshot);
        let markdown_import = directory.path().join("markdown-import.mdtree");
        import_markdown_snapshot_new(&markdown_path, &markdown_import).expect("Markdown import");
        let markdown_store = SqliteStore::open(&markdown_import).expect("open Markdown import");
        assert_eq!(
            export_snapshot(&markdown_store).expect("export Markdown import"),
            plan.snapshot
        );
        assert!(!first_manifest.is_empty());

        let child_dir = std::fs::read_dir(&markdown_path)
            .expect("read export")
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .expect("child directory")
            .path();
        assert_eq!(
            child_dir.file_name().and_then(|name| name.to_str()),
            Some("0000000000-orders--01JZ8Q5CWPN8T7KPN5A1V9B6XN")
        );
        let child_file = child_dir.join("node.md");
        let malformed = std::fs::read_to_string(&child_file)
            .expect("child file")
            .replace(&format!("parent_id: {}", root.id()), "parent_id: null");
        std::fs::write(&child_file, malformed).expect("corrupt hierarchy");
        let untouched = directory.path().join("untouched.mdtree");
        assert!(import_markdown_snapshot_new(&markdown_path, &untouched).is_err());
        assert!(!untouched.exists());
    }
}
