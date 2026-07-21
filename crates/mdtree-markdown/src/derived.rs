//! Pure construction of all data derived from one canonical node.

use std::collections::BTreeMap;
use std::str::FromStr;

use mdtree_core::{
    hash_content, Node, NodeHash, Reference, ReferenceOrigin, ReferenceTarget, ReferenceType,
    Section, UlidGenerator,
};

use crate::{extract_markdown_links, extract_wikilinks, parse_sections, LinkKind, MarkdownError};

/// Search-index representation of one semantic section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsDocument {
    /// Derived section identity.
    pub section_id: mdtree_core::NodeId,
    /// Canonical containing node.
    pub node_id: mdtree_core::NodeId,
    /// Node title.
    pub title: String,
    /// Space-separated aliases.
    pub aliases: String,
    /// Optional node summary.
    pub summary: String,
    /// Optional section heading.
    pub heading: String,
    /// Section Markdown content.
    pub content: String,
    /// Space-separated tags.
    pub tags: String,
    /// Space-separated keywords.
    pub keywords: String,
}

/// Complete rebuildable derived state for one node.
#[derive(Clone, Debug, PartialEq)]
pub struct DerivedNodeRecords {
    /// Exact canonical content hash.
    pub content_hash: NodeHash,
    /// Heading-oriented sections.
    pub sections: Vec<Section>,
    /// Extracted standard links and wikilinks.
    pub references: Vec<Reference>,
    /// One FTS document per section.
    pub fts_documents: Vec<FtsDocument>,
}

/// Parses and builds all derived state without performing I/O.
///
/// # Errors
///
/// Returns [`MarkdownError`] if section offsets cannot be represented.
pub fn build_derived_records(
    node: &Node,
    ids: &dyn UlidGenerator,
) -> Result<DerivedNodeRecords, MarkdownError> {
    let fields = node.fields();
    let sections = parse_sections(node.id(), &fields.markdown_content, ids)?;
    let mut references = Vec::new();

    for link in extract_markdown_links(&fields.markdown_content, &sections) {
        references.push(Reference {
            source_node_id: node.id(),
            source_section_id: link.source_section_id,
            reference_type: ReferenceType::from_str(match link.kind {
                LinkKind::Internal => "markdown_link",
                LinkKind::External => "external_link",
            })?,
            target: ReferenceTarget::Unresolved {
                target_ref: link.destination,
            },
            origin: ReferenceOrigin::Markdown,
            metadata: BTreeMap::new(),
        });
    }
    for link in extract_wikilinks(&fields.markdown_content, &sections) {
        let target_ref = link.anchor.map_or(link.target.clone(), |anchor| {
            format!("{}#{anchor}", link.target)
        });
        references.push(Reference {
            source_node_id: node.id(),
            source_section_id: link.source_section_id,
            reference_type: ReferenceType::from_str("references")?,
            target: ReferenceTarget::Unresolved { target_ref },
            origin: ReferenceOrigin::Wikilink,
            metadata: BTreeMap::new(),
        });
    }

    let fts_documents = sections
        .iter()
        .map(|section| FtsDocument {
            section_id: section.id,
            node_id: node.id(),
            title: fields.metadata.title.clone(),
            aliases: fields.metadata.aliases.join(" "),
            summary: fields.metadata.summary.clone().unwrap_or_default(),
            heading: section.heading.clone().unwrap_or_default(),
            content: section.content.clone(),
            tags: fields.metadata.tags.join(" "),
            keywords: fields.metadata.keywords.join(" "),
        })
        .collect();

    Ok(DerivedNodeRecords {
        content_hash: hash_content(&fields.markdown_content),
        sections,
        references,
        fts_documents,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        Node, NodeFields, NodeHash, NodeId, NodeMetadata, SequentialUlidGenerator, Slug,
    };

    use super::build_derived_records;

    #[test]
    fn golden_fixture_is_stable_and_complete() {
        let mut metadata = NodeMetadata::new("Database Models");
        metadata.aliases.push("DB Models".into());
        metadata.keywords.push("schema".into());
        let node = Node::new(
            NodeFields {
                id: NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("fixture ID"),
                slug: Slug::from_str("database-models").expect("fixture slug"),
                metadata,
                markdown_content: "# Models\nSee [[Products]] and [API](../api.md).\n".into(),
                sibling_order: 0,
                version: 1,
                content_hash: NodeHash::new([0; 32]),
                revision_hash: NodeHash::new([0; 32]),
                created_at: 1,
                updated_at: 1,
            },
            Some(NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XN").expect("parent ID")),
        )
        .expect("node");

        let first =
            build_derived_records(&node, &SequentialUlidGenerator::new(100)).expect("derived");
        let second =
            build_derived_records(&node, &SequentialUlidGenerator::new(100)).expect("derived");
        assert_eq!(first, second);
        assert_eq!(first.sections.len(), 1);
        assert_eq!(first.references.len(), 2);
        assert_eq!(first.fts_documents[0].title, "Database Models");
        assert_eq!(first.fts_documents[0].keywords, "schema");
    }
}
