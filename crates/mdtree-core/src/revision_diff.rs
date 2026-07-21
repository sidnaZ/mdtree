//! Deterministic metadata and Markdown revision comparisons.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::NodeRevision;

/// Canonical field category changed between revisions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionField {
    /// Descriptive metadata.
    Metadata,
    /// Canonical slug.
    Slug,
    /// Canonical parent.
    Parent,
    /// Sibling order.
    SiblingOrder,
    /// Markdown content.
    Markdown,
}

/// Human- and machine-usable comparison of two immutable revisions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RevisionDiff {
    /// Whether any canonical state differs.
    pub changed: bool,
    /// Exact canonical field categories that differ.
    pub changed_fields: BTreeSet<RevisionField>,
    /// Simple deterministic line-oriented Markdown diff.
    pub markdown_diff: String,
}

/// Compares any two retained node revisions.
#[must_use]
pub fn diff_revisions(from: &NodeRevision, to: &NodeRevision) -> RevisionDiff {
    let markdown_diff = line_diff(&from.markdown_content, &to.markdown_content);
    let mut changed_fields = BTreeSet::new();
    if from.metadata != to.metadata {
        changed_fields.insert(RevisionField::Metadata);
    }
    if from.slug != to.slug {
        changed_fields.insert(RevisionField::Slug);
    }
    if from.parent_id != to.parent_id {
        changed_fields.insert(RevisionField::Parent);
    }
    if from.sibling_order != to.sibling_order {
        changed_fields.insert(RevisionField::SiblingOrder);
    }
    if !markdown_diff.is_empty() {
        changed_fields.insert(RevisionField::Markdown);
    }
    RevisionDiff {
        changed: !changed_fields.is_empty(),
        changed_fields,
        markdown_diff,
    }
}

fn line_diff(from: &str, to: &str) -> String {
    if from == to {
        return String::new();
    }
    let mut output = String::new();
    for line in from.lines() {
        output.push_str("- ");
        output.push_str(line);
        output.push('\n');
    }
    for line in to.lines() {
        output.push_str("+ ");
        output.push_str(line);
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{NodeHash, NodeId, NodeMetadata, NodeRevision, Slug};

    use super::{diff_revisions, RevisionField};

    fn revision() -> NodeRevision {
        NodeRevision {
            node_id: NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("fixture ID"),
            parent_id: None,
            slug: Slug::from_str("node").expect("fixture slug"),
            metadata: NodeMetadata::new("Node"),
            markdown_content: "# Node\n".into(),
            sibling_order: 0,
            version: 1,
            content_hash: NodeHash::new([1; 32]),
            revision_hash: NodeHash::new([2; 32]),
            change_summary: None,
            created_by: None,
            created_at: 1,
        }
    }

    #[test]
    fn covers_content_metadata_rename_and_no_op_comparisons() {
        let original = revision();
        assert!(!diff_revisions(&original, &original).changed);

        let mut content = original.clone();
        content.markdown_content = "# Changed\n".into();
        assert!(!diff_revisions(&original, &content).markdown_diff.is_empty());

        let mut metadata = original.clone();
        metadata.metadata.summary = Some("Summary".into());
        assert!(diff_revisions(&original, &metadata)
            .changed_fields
            .contains(&RevisionField::Metadata));

        let mut renamed = original.clone();
        renamed.slug = Slug::from_str("renamed").expect("fixture slug");
        assert!(diff_revisions(&original, &renamed)
            .changed_fields
            .contains(&RevisionField::Slug));
    }
}
