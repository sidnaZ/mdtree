//! Deterministic structural and canonical-content subtree comparison.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{diff_revisions, NodeId, NodeRevision, RevisionDiff, RevisionField, SnapshotNode};

/// Structural or canonical-state change represented by one subtree diff item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtreeChange {
    /// The relative position exists only in the right subtree.
    Added,
    /// The relative position exists only in the left subtree.
    Removed,
    /// One stable node occurs at different relative paths.
    Moved,
    /// Paired canonical state differs.
    Changed,
}

/// One deterministic structural/content difference between two subtrees.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubtreeDiffItem {
    /// Stable identity in the left subtree, when present.
    pub from_node_id: Option<NodeId>,
    /// Stable identity in the right subtree, when present.
    pub to_node_id: Option<NodeId>,
    /// Canonical path relative to the left root (`.` for the root).
    pub from_path: Option<String>,
    /// Canonical path relative to the right root (`.` for the root).
    pub to_path: Option<String>,
    /// All structural and canonical-state change categories for this item.
    pub changes: BTreeSet<SubtreeChange>,
    /// Canonical field comparison when both sides are paired.
    pub revision_diff: Option<RevisionDiff>,
}

/// Compares two complete current subtree projections.
///
/// Stable identities are paired first, allowing overlapping subtree views to
/// report moves. Remaining nodes are paired by relative canonical path so two
/// independently-created but structurally equivalent trees can be compared.
/// Unpaired positions are reported as additions or removals.
#[must_use]
pub fn diff_subtrees(
    from_root: NodeId,
    from_nodes: &[SnapshotNode],
    to_root: NodeId,
    to_nodes: &[SnapshotNode],
) -> Vec<SubtreeDiffItem> {
    let from_paths = relative_paths(from_root, from_nodes);
    let to_paths = relative_paths(to_root, to_nodes);
    let from_by_id = from_nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let to_by_id = to_nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut paired_from = BTreeSet::new();
    let mut paired_to = BTreeSet::new();
    let mut items = Vec::new();

    for (id, from) in &from_by_id {
        let Some(to) = to_by_id.get(id) else { continue };
        paired_from.insert(*id);
        paired_to.insert(*id);
        if let Some(item) = paired_item(
            from,
            to,
            from_paths.get(id).cloned(),
            to_paths.get(id).cloned(),
            false,
        ) {
            items.push(item);
        }
    }

    let unmatched_to_by_path = to_nodes
        .iter()
        .filter(|node| !paired_to.contains(&node.id))
        .filter_map(|node| to_paths.get(&node.id).cloned().map(|path| (path, node)))
        .collect::<BTreeMap<_, _>>();
    let unmatched_from = from_nodes
        .iter()
        .filter(|node| !paired_from.contains(&node.id))
        .collect::<Vec<_>>();
    for from in unmatched_from {
        let Some(path) = from_paths.get(&from.id) else {
            continue;
        };
        let Some(to) = unmatched_to_by_path.get(path) else {
            continue;
        };
        paired_from.insert(from.id);
        paired_to.insert(to.id);
        if let Some(item) = paired_item(from, to, Some(path.clone()), Some(path.clone()), true) {
            items.push(item);
        }
    }

    for node in from_nodes
        .iter()
        .filter(|node| !paired_from.contains(&node.id))
    {
        items.push(SubtreeDiffItem {
            from_node_id: Some(node.id),
            to_node_id: None,
            from_path: from_paths.get(&node.id).cloned(),
            to_path: None,
            changes: BTreeSet::from([SubtreeChange::Removed]),
            revision_diff: None,
        });
    }
    for node in to_nodes.iter().filter(|node| !paired_to.contains(&node.id)) {
        items.push(SubtreeDiffItem {
            from_node_id: None,
            to_node_id: Some(node.id),
            from_path: None,
            to_path: to_paths.get(&node.id).cloned(),
            changes: BTreeSet::from([SubtreeChange::Added]),
            revision_diff: None,
        });
    }
    items.sort_by(|left, right| {
        left.from_path
            .as_deref()
            .unwrap_or(left.to_path.as_deref().unwrap_or(""))
            .cmp(
                right
                    .from_path
                    .as_deref()
                    .unwrap_or(right.to_path.as_deref().unwrap_or("")),
            )
            .then_with(|| left.to_path.cmp(&right.to_path))
            .then_with(|| left.from_node_id.cmp(&right.from_node_id))
            .then_with(|| left.to_node_id.cmp(&right.to_node_id))
    });
    items
}

fn paired_item(
    from: &SnapshotNode,
    to: &SnapshotNode,
    from_path: Option<String>,
    to_path: Option<String>,
    ignore_parent: bool,
) -> Option<SubtreeDiffItem> {
    let mut revision_diff = diff_revisions(&as_revision(from), &as_revision(to));
    if ignore_parent {
        revision_diff.changed_fields.remove(&RevisionField::Parent);
        revision_diff.changed = !revision_diff.changed_fields.is_empty();
    }
    let moved = from_path != to_path;
    let mut changes = BTreeSet::new();
    if moved {
        changes.insert(SubtreeChange::Moved);
    }
    if revision_diff.changed {
        changes.insert(SubtreeChange::Changed);
    }
    if changes.is_empty() {
        return None;
    }
    Some(SubtreeDiffItem {
        from_node_id: Some(from.id),
        to_node_id: Some(to.id),
        from_path,
        to_path,
        changes,
        revision_diff: Some(revision_diff),
    })
}

fn as_revision(node: &SnapshotNode) -> NodeRevision {
    NodeRevision {
        node_id: node.id,
        parent_id: node.parent_id,
        slug: node.slug.clone(),
        metadata: node.metadata.clone(),
        markdown_content: node.markdown_content.clone(),
        sibling_order: node.sibling_order,
        version: node.version,
        content_hash: node.content_hash,
        revision_hash: node.revision_hash,
        change_summary: None,
        created_by: None,
        created_at: node.updated_at,
    }
}

fn relative_paths(root: NodeId, nodes: &[SnapshotNode]) -> BTreeMap<NodeId, String> {
    let by_id = nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    nodes
        .iter()
        .map(|node| {
            let mut current = node;
            let mut segments = Vec::new();
            let mut visited = BTreeSet::new();
            while current.id != root && visited.insert(current.id) {
                segments.push(current.slug.as_str().to_owned());
                let Some(parent) = current.parent_id.and_then(|id| by_id.get(&id).copied()) else {
                    break;
                };
                current = parent;
            }
            segments.reverse();
            (
                node.id,
                if segments.is_empty() {
                    ".".into()
                } else {
                    segments.join("/")
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{NodeHash, NodeMetadata, Slug};

    use super::{diff_subtrees, SubtreeChange};

    fn node(id: &str, parent_id: Option<&str>, slug: &str, markdown: &str) -> crate::SnapshotNode {
        crate::SnapshotNode {
            id: crate::NodeId::from_str(id).expect("ID"),
            parent_id: parent_id.map(|id| crate::NodeId::from_str(id).expect("parent ID")),
            slug: Slug::from_str(slug).expect("slug"),
            metadata: NodeMetadata::new(slug),
            markdown_content: markdown.into(),
            sibling_order: 0,
            version: 1,
            content_hash: NodeHash::new([1; 32]),
            revision_hash: NodeHash::new([2; 32]),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn reports_unchanged_changed_added_removed_and_moved_nodes() {
        const LEFT: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
        const RIGHT: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XN";
        const SHARED: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XP";
        const OLD: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XQ";
        const NEW: &str = "01JZ8Q5CWPN8T7KPN5A1V9B6XR";
        let left = vec![
            node(LEFT, None, "left", "same"),
            node(SHARED, Some(LEFT), "shared", "same"),
            node(OLD, Some(LEFT), "paired", "old"),
        ];
        let right = vec![
            node(RIGHT, None, "right", "same"),
            node(SHARED, Some(RIGHT), "nested", "same"),
            node(NEW, Some(RIGHT), "paired", "new"),
        ];
        let diff = diff_subtrees(left[0].id, &left, right[0].id, &right);
        assert!(diff
            .iter()
            .any(|item| item.changes.contains(&SubtreeChange::Moved)));
        assert!(diff
            .iter()
            .any(|item| item.changes.contains(&SubtreeChange::Changed)));

        let removed = vec![node(OLD, Some(LEFT), "removed", "old")];
        let added = vec![node(NEW, Some(RIGHT), "added", "new")];
        let structural = diff_subtrees(left[0].id, &removed, right[0].id, &added);
        assert!(structural
            .iter()
            .any(|item| item.changes.contains(&SubtreeChange::Removed)));
        assert!(structural
            .iter()
            .any(|item| item.changes.contains(&SubtreeChange::Added)));
        assert!(diff_subtrees(left[0].id, &left, left[0].id, &left).is_empty());
    }
}
