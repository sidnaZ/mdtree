//! Lossless deterministic Markdown snapshot directories.

#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use mdtree_core::{
    NodeHash, NodeId, NodeMetadata, NodeRevision, Reference, RevisionPolicy, Slug, Snapshot,
    SnapshotNode, SnapshotWorkspace,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MANIFEST: &str = "workspace.yaml";
const NODE_FILE: &str = "node.md";

/// Markdown snapshot filesystem or serialization failure.
#[derive(Debug, Error)]
pub enum MarkdownSnapshotError {
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// YAML frontmatter or manifest is invalid.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// Snapshot layout is internally inconsistent.
    #[error("invalid Markdown snapshot layout: {0}")]
    Layout(String),
}

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    format: String,
    format_version: u32,
    workspace: SnapshotWorkspace,
    revision_policy: RevisionPolicy,
    revisions: Vec<NodeRevision>,
    references: Vec<Reference>,
}

#[derive(Debug, Deserialize, Serialize)]
struct NodeFrontmatter {
    id: NodeId,
    parent_id: Option<NodeId>,
    slug: Slug,
    sibling_order: u32,
    version: u64,
    content_hash: NodeHash,
    revision_hash: NodeHash,
    created_at: u64,
    updated_at: u64,
    metadata: NodeMetadata,
}

/// Writes a complete snapshot as a deterministic directory tree.
pub fn export_markdown_snapshot(
    path: &Path,
    snapshot: &Snapshot,
) -> Result<(), MarkdownSnapshotError> {
    if path.exists() {
        return Err(MarkdownSnapshotError::Layout(format!(
            "destination {} already exists",
            path.display()
        )));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::tempdir_in(parent)?;
    let output = temporary.path().join("snapshot");
    fs::create_dir(&output)?;
    write_yaml(
        &output.join(MANIFEST),
        &Manifest {
            format: snapshot.format.clone(),
            format_version: snapshot.format_version,
            workspace: snapshot.workspace.clone(),
            revision_policy: snapshot.revision_policy,
            revisions: snapshot.revisions.clone(),
            references: snapshot.references.clone(),
        },
    )?;
    let by_parent = children_by_parent(snapshot);
    let root = by_parent
        .get(&None)
        .and_then(|nodes| nodes.first())
        .ok_or_else(|| MarkdownSnapshotError::Layout("snapshot has no root node".into()))?;
    write_node_tree(&output, root, &by_parent)?;
    fs::rename(output, path)?;
    Ok(())
}

/// Writes one canonical node and its included descendants to a file target or directory.
///
/// An existing directory receives `<slug>.md`; a nonexistent target is used verbatim as the
/// selected node's file. Every descendant is written as `<slug>/<slug>.md` beside that root file
/// and below its parent, preserving tree depth in the filesystem.
pub fn export_markdown_subtree(
    path: &Path,
    nodes: &[SnapshotNode],
) -> Result<Vec<PathBuf>, MarkdownSnapshotError> {
    let root = nodes
        .first()
        .ok_or_else(|| MarkdownSnapshotError::Layout("node export has no root node".into()))?;
    let (output_directory, root_file) = if path.is_dir() {
        (path.to_path_buf(), path.join(format!("{}.md", root.slug)))
    } else {
        (
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            path.to_path_buf(),
        )
    };

    let mut by_parent: HashMap<Option<NodeId>, Vec<&SnapshotNode>> = HashMap::new();
    for node in nodes.iter().skip(1) {
        by_parent.entry(node.parent_id).or_default().push(node);
    }
    for children in by_parent.values_mut() {
        children.sort_by_key(|node| (node.sibling_order, node.slug.clone(), node.id));
    }
    validate_standalone_tree(root, nodes, &by_parent)?;

    if let Some(children) = by_parent.get(&Some(root.id)) {
        for child in children {
            let child_directory = output_directory.join(child.slug.as_str());
            if child_directory == root_file {
                return Err(MarkdownSnapshotError::Layout(format!(
                    "root file {} conflicts with descendant directory",
                    root_file.display()
                )));
            }
            if child_directory.exists() && !child_directory.is_dir() {
                return Err(MarkdownSnapshotError::Layout(format!(
                    "destination {} is not a directory",
                    child_directory.display()
                )));
            }
        }
    }

    fs::create_dir_all(&output_directory)?;
    write_file(&root_file, &render_node(root)?)?;
    let mut exported_files = vec![root_file];
    if let Some(children) = by_parent.get(&Some(root.id)) {
        for child in children {
            let child_directory = output_directory.join(child.slug.as_str());
            fs::create_dir_all(&child_directory)?;
            write_standalone_tree(&child_directory, child, &by_parent, &mut exported_files)?;
        }
    }
    Ok(exported_files)
}

fn validate_standalone_tree(
    root: &SnapshotNode,
    nodes: &[SnapshotNode],
    by_parent: &HashMap<Option<NodeId>, Vec<&SnapshotNode>>,
) -> Result<(), MarkdownSnapshotError> {
    let ids = nodes
        .iter()
        .map(|node| node.id)
        .collect::<std::collections::HashSet<_>>();
    if ids.len() != nodes.len() {
        return Err(MarkdownSnapshotError::Layout(
            "node export contains duplicate node IDs".into(),
        ));
    }
    for node in nodes.iter().skip(1) {
        if !node.parent_id.is_some_and(|parent| ids.contains(&parent)) {
            return Err(MarkdownSnapshotError::Layout(format!(
                "node {} is not connected to export root {}",
                node.id, root.id
            )));
        }
    }
    for children in by_parent.values() {
        let mut slugs = std::collections::HashSet::new();
        for child in children {
            if !slugs.insert(child.slug.clone()) {
                return Err(MarkdownSnapshotError::Layout(format!(
                    "duplicate exported sibling slug {}",
                    child.slug
                )));
            }
        }
    }
    Ok(())
}

fn write_standalone_tree(
    directory: &Path,
    node: &SnapshotNode,
    by_parent: &HashMap<Option<NodeId>, Vec<&SnapshotNode>>,
    exported_files: &mut Vec<PathBuf>,
) -> Result<(), MarkdownSnapshotError> {
    let node_file = directory.join(format!("{}.md", node.slug));
    write_file(&node_file, &render_node(node)?)?;
    exported_files.push(node_file);
    if let Some(children) = by_parent.get(&Some(node.id)) {
        for child in children {
            let child_directory = directory.join(child.slug.as_str());
            fs::create_dir_all(&child_directory)?;
            write_standalone_tree(&child_directory, child, by_parent, exported_files)?;
        }
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<(), MarkdownSnapshotError> {
    fs::write(path, contents)?;
    Ok(())
}

fn children_by_parent(snapshot: &Snapshot) -> HashMap<Option<NodeId>, Vec<&SnapshotNode>> {
    let mut result: HashMap<_, Vec<_>> = HashMap::new();
    for node in &snapshot.nodes {
        result.entry(node.parent_id).or_default().push(node);
    }
    for nodes in result.values_mut() {
        nodes.sort_by_key(|node| (node.sibling_order, node.slug.clone(), node.id));
    }
    result
}

fn write_node_tree(
    directory: &Path,
    node: &SnapshotNode,
    by_parent: &HashMap<Option<NodeId>, Vec<&SnapshotNode>>,
) -> Result<(), MarkdownSnapshotError> {
    fs::write(directory.join(NODE_FILE), render_node(node)?)?;
    if let Some(children) = by_parent.get(&Some(node.id)) {
        for child in children {
            let child_directory = directory.join(format!(
                "{:010}-{}--{}",
                child.sibling_order, child.slug, child.id
            ));
            fs::create_dir(&child_directory)?;
            write_node_tree(&child_directory, child, by_parent)?;
        }
    }
    Ok(())
}

fn render_node(node: &SnapshotNode) -> Result<String, MarkdownSnapshotError> {
    let frontmatter = NodeFrontmatter {
        id: node.id,
        parent_id: node.parent_id,
        slug: node.slug.clone(),
        sibling_order: node.sibling_order,
        version: node.version,
        content_hash: node.content_hash,
        revision_hash: node.revision_hash,
        created_at: node.created_at,
        updated_at: node.updated_at,
        metadata: node.metadata.clone(),
    };
    let yaml = serde_yaml::to_string(&frontmatter)?;
    Ok(format!("---\n{yaml}---\n{}", node.markdown_content))
}

fn write_yaml<T: Serialize>(path: &Path, value: &T) -> Result<(), MarkdownSnapshotError> {
    fs::write(path, serde_yaml::to_string(value)?)?;
    Ok(())
}

/// Parses the complete directory tree without modifying any workspace.
pub fn parse_markdown_snapshot(path: &Path) -> Result<Snapshot, MarkdownSnapshotError> {
    let manifest: Manifest = serde_yaml::from_slice(&fs::read(path.join(MANIFEST))?)?;
    let mut nodes = Vec::new();
    read_node_tree(path, None, &mut nodes)?;
    Ok(Snapshot {
        format: manifest.format,
        format_version: manifest.format_version,
        workspace: manifest.workspace,
        revision_policy: manifest.revision_policy,
        nodes,
        revisions: manifest.revisions,
        references: manifest.references,
    })
}

fn read_node_tree(
    directory: &Path,
    expected_parent: Option<NodeId>,
    nodes: &mut Vec<SnapshotNode>,
) -> Result<(), MarkdownSnapshotError> {
    let node_path = directory.join(NODE_FILE);
    let text = fs::read_to_string(&node_path)?;
    let (frontmatter, markdown_content) = split_node(&text, &node_path)?;
    let node: NodeFrontmatter = serde_yaml::from_str(frontmatter)?;
    if node.parent_id != expected_parent {
        return Err(MarkdownSnapshotError::Layout(format!(
            "{} declares parent {:?}, but directory hierarchy requires {:?}",
            node_path.display(),
            node.parent_id,
            expected_parent
        )));
    }
    if expected_parent.is_some() {
        let actual_name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| MarkdownSnapshotError::Layout("non-Unicode directory name".into()))?;
        let expected_name = format!("{:010}-{}--{}", node.sibling_order, node.slug, node.id);
        if actual_name != expected_name {
            return Err(MarkdownSnapshotError::Layout(format!(
                "directory {actual_name} must be named {expected_name}"
            )));
        }
    }
    let id = node.id;
    nodes.push(SnapshotNode {
        id,
        parent_id: node.parent_id,
        slug: node.slug,
        metadata: node.metadata,
        markdown_content: markdown_content.to_owned(),
        sibling_order: node.sibling_order,
        version: node.version,
        content_hash: node.content_hash,
        revision_hash: node.revision_hash,
        created_at: node.created_at,
        updated_at: node.updated_at,
    });
    let mut children = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<PathBuf>>();
    children.sort();
    for child in children {
        read_node_tree(&child, Some(id), nodes)?;
    }
    Ok(())
}

fn split_node<'a>(text: &'a str, path: &Path) -> Result<(&'a str, &'a str), MarkdownSnapshotError> {
    let rest = text.strip_prefix("---\n").ok_or_else(|| {
        MarkdownSnapshotError::Layout(format!("{} has no YAML frontmatter", path.display()))
    })?;
    let (yaml, markdown) = rest.split_once("---\n").ok_or_else(|| {
        MarkdownSnapshotError::Layout(format!("{} has unterminated frontmatter", path.display()))
    })?;
    Ok((yaml, markdown))
}
