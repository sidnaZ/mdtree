//! Deterministic bounded inspection and read/write context assembly.

#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use mdtree_core::{
    Breadcrumb, ContextSummary, ConventionSummary, InspectionItem, Node, NodeDepthProjection,
    NodeId, PageCursor, PageLimit, ReadContext, SubtreeInspection, WriteContext,
};
use rusqlite::params_from_iter;
use serde::Serialize;

use crate::{PageReadError, SqliteStore, StoreError};

impl SqliteStore {
    /// Returns a stable tree view bounded by relative depth and item count.
    pub fn inspect_subtree_page(
        &self,
        operation: &str,
        id: NodeId,
        max_depth: u32,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<SubtreeInspection, PageReadError> {
        let page = self.inspection_traversal_page(operation, id, max_depth, limit, cursor)?;
        let ids = page.items.iter().map(|row| row.node.id).collect::<Vec<_>>();
        let counts = self.child_counts(&ids)?;
        let count_by_id = counts
            .into_iter()
            .map(|count| (count.node_id, count.child_count))
            .collect::<BTreeMap<_, _>>();
        let depth_truncated = page.items.iter().any(|row| {
            row.depth == max_depth
                && count_by_id
                    .get(&row.node.id)
                    .is_some_and(|count| *count > 0)
        });
        let breadcrumbs = self.inspection_breadcrumbs(id, &page.items)?;

        let mut items = Vec::with_capacity(page.items.len());
        for row in page.items {
            let breadcrumb = breadcrumbs
                .get(&row.node.id)
                .cloned()
                .ok_or_else(|| StoreError::InvalidData("inspection breadcrumb".into()))?;
            items.push(InspectionItem {
                child_count: count_by_id.get(&row.node.id).copied().unwrap_or(0),
                node: ContextSummary {
                    node_id: row.node.id,
                    title: row.node.metadata.title,
                    summary: row.node.metadata.summary,
                    node_type: row.node.metadata.node_type,
                    breadcrumb,
                },
                depth: row.depth,
            });
        }
        Ok(SubtreeInspection {
            items,
            next_cursor: page.next_cursor,
            truncated: page.truncated || depth_truncated,
        })
    }

    fn inspection_breadcrumbs(
        &self,
        root_id: NodeId,
        rows: &[NodeDepthProjection],
    ) -> Result<BTreeMap<NodeId, Breadcrumb>, StoreError> {
        let page_ids = rows.iter().map(|row| row.node.id).collect::<BTreeSet<_>>();
        let external_parents = rows
            .iter()
            .filter(|row| row.depth > 0)
            .filter_map(|row| row.node.parent_id)
            .filter(|parent| !page_ids.contains(parent))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut breadcrumbs = self.breadcrumb_map(&external_parents)?;
        if rows.iter().any(|row| row.depth == 0) {
            breadcrumbs.insert(root_id, self.breadcrumb(root_id)?);
        }
        for row in rows {
            if breadcrumbs.contains_key(&row.node.id) {
                continue;
            }
            let parent = row
                .node
                .parent_id
                .and_then(|parent| breadcrumbs.get(&parent))
                .ok_or_else(|| StoreError::InvalidData("inspection parent breadcrumb".into()))?;
            let mut titles = parent.segments().to_vec();
            titles.push(row.node.metadata.title.clone());
            breadcrumbs.insert(
                row.node.id,
                Breadcrumb::new(titles)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            );
        }
        Ok(breadcrumbs)
    }

    fn breadcrumb_map(&self, ids: &[NodeId]) -> Result<BTreeMap<NodeId, Breadcrumb>, StoreError> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let placeholders = (1..=ids.len())
            .map(|position| format!("?{position}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH RECURSIVE ancestry(requested_id,id,parent_id,title,depth) AS (
             SELECT id,id,parent_id,title,0 FROM nodes WHERE id IN ({placeholders})
             UNION ALL
             SELECT ancestry.requested_id,parent.id,parent.parent_id,parent.title,ancestry.depth+1
             FROM nodes parent JOIN ancestry ON parent.id=ancestry.parent_id
             ) SELECT requested_id,title FROM ancestry ORDER BY requested_id,depth DESC"
        );
        let parameters = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut statement = self.connection().prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut segments = BTreeMap::<NodeId, Vec<String>>::new();
        for row in rows {
            let (requested_id, title) = row?;
            let requested_id = NodeId::from_str(&requested_id)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?;
            segments.entry(requested_id).or_default().push(title);
        }
        segments
            .into_iter()
            .map(|(node_id, titles)| {
                Breadcrumb::new(titles)
                    .map(|breadcrumb| (node_id, breadcrumb))
                    .map_err(|error| StoreError::InvalidData(error.to_string()))
            })
            .collect()
    }

    /// Assembles prioritized read context under an exact serialized JSON byte limit.
    pub fn read_context(&self, id: NodeId, byte_limit: usize) -> Result<ReadContext, StoreError> {
        let node = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let ancestors = self
            .ancestors(id)?
            .into_iter()
            .map(|item| self.summary(&item.node))
            .collect::<Result<Vec<_>, _>>()?;
        let children = self
            .children(id)?
            .iter()
            .map(|child| self.summary(child))
            .collect::<Result<Vec<_>, _>>()?;
        let mut context = ReadContext {
            node: self.summary(&node)?,
            ancestors,
            content: Some(node.fields().markdown_content.clone()),
            children,
            references: self.outgoing_references(id)?,
            omitted: Vec::new(),
            truncated: false,
            estimated_tokens: 0,
        };
        while serialized_len(&mut context)? > byte_limit {
            context.truncated = true;
            if !context.references.is_empty() {
                context.references.clear();
                push_omitted(&mut context.omitted, "references");
                continue;
            }
            if !context.children.is_empty() {
                context.children.clear();
                push_omitted(&mut context.omitted, "children");
                continue;
            }
            if context.content.take().is_some() {
                push_omitted(&mut context.omitted, "content");
                continue;
            }
            if !context.ancestors.is_empty() {
                context.ancestors.pop();
                push_omitted(&mut context.omitted, "ancestor summaries");
                continue;
            }
            compact_omissions(&mut context.omitted);
            let minimum = serialized_len(&mut context)?;
            return Err(StoreError::BudgetExceeded {
                minimum,
                requested: byte_limit,
            });
        }
        Ok(context)
    }

    /// Assembles optimistic-write state and local conventions under an exact byte limit.
    pub fn write_context(&self, id: NodeId, byte_limit: usize) -> Result<WriteContext, StoreError> {
        let node = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let sibling_examples = self
            .siblings(id)?
            .into_iter()
            .filter(|sibling| sibling.id() != id)
            .map(|sibling| self.summary(&sibling))
            .collect::<Result<Vec<_>, _>>()?;
        let ancestor_conventions = self
            .ancestors(id)?
            .into_iter()
            .map(|item| {
                Ok(ConventionSummary {
                    node: self.summary(&item.node)?,
                    owns: item.node.fields().metadata.owns.clone(),
                    excludes: item.node.fields().metadata.excludes.clone(),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let fields = node.fields();
        let mut context = WriteContext {
            target: self.summary(&node)?,
            version: fields.version,
            content_hash: fields.content_hash,
            owns: fields.metadata.owns.clone(),
            excludes: fields.metadata.excludes.clone(),
            accepts_children: fields.metadata.accepts_children.clone(),
            sibling_examples,
            ancestor_conventions,
            references: self.outgoing_references(id)?,
            omitted: Vec::new(),
            truncated: false,
            estimated_tokens: 0,
        };
        while serialized_len(&mut context)? > byte_limit {
            context.truncated = true;
            if !context.references.is_empty() {
                context.references.clear();
                push_omitted(&mut context.omitted, "references");
                continue;
            }
            if !context.sibling_examples.is_empty() {
                context.sibling_examples.pop();
                push_omitted(&mut context.omitted, "sibling examples");
                continue;
            }
            if !context.ancestor_conventions.is_empty() {
                context.ancestor_conventions.pop();
                push_omitted(&mut context.omitted, "ancestor conventions");
                continue;
            }
            compact_omissions(&mut context.omitted);
            let minimum = serialized_len(&mut context)?;
            return Err(StoreError::BudgetExceeded {
                minimum,
                requested: byte_limit,
            });
        }
        Ok(context)
    }

    pub(crate) fn summary(&self, node: &Node) -> Result<ContextSummary, StoreError> {
        Ok(ContextSummary {
            node_id: node.id(),
            title: node.fields().metadata.title.clone(),
            summary: node.fields().metadata.summary.clone(),
            node_type: node.fields().metadata.node_type.clone(),
            breadcrumb: self.breadcrumb(node.id())?,
        })
    }
}

fn serialized_len<T: Serialize + TokenEstimate>(value: &mut T) -> Result<usize, StoreError> {
    for _ in 0..3 {
        let length = serde_json::to_vec(value)?.len();
        let estimate = u64::try_from(length.div_ceil(4)).unwrap_or(u64::MAX);
        if value.estimated_tokens() == estimate {
            return Ok(length);
        }
        value.set_estimated_tokens(estimate);
    }
    Ok(serde_json::to_vec(value)?.len())
}
trait TokenEstimate {
    fn estimated_tokens(&self) -> u64;
    fn set_estimated_tokens(&mut self, value: u64);
}
impl TokenEstimate for ReadContext {
    fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }
    fn set_estimated_tokens(&mut self, value: u64) {
        self.estimated_tokens = value;
    }
}
impl TokenEstimate for WriteContext {
    fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }
    fn set_estimated_tokens(&mut self, value: u64) {
        self.estimated_tokens = value;
    }
}
fn push_omitted(omitted: &mut Vec<String>, value: &str) {
    if !omitted.iter().any(|item| item == value) {
        omitted.push(value.into());
    }
}
fn compact_omissions(omitted: &mut Vec<String>) {
    omitted.clear();
    omitted.push("additional context omitted".into());
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        hash_content, hash_revision, Node, NodeFields, NodeId, NodeMetadata, NodeRevision,
        PageLimit, RevisionHashInput, SequentialUlidGenerator, Slug,
    };
    use mdtree_markdown::build_derived_records;
    use tempfile::tempdir;

    use crate::{create_workspace, SqliteStore, StoreError};

    fn id(value: &str) -> NodeId {
        NodeId::from_str(value).expect("ID")
    }
    fn node(raw: &str, parent: Option<NodeId>, title: &str, slug: &str, order: u32) -> Node {
        let node_id = id(raw);
        let slug = Slug::from_str(slug).expect("slug");
        let mut metadata = NodeMetadata::new(title);
        metadata.summary = Some(format!("Summary for {title}"));
        if title == "Target" {
            metadata.owns.push("database definitions".into());
            metadata.excludes.push("API contracts".into());
        }
        let content = format!("# {title}\nDetailed content for {title}.\n");
        let content_hash = hash_content(&content);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id,
            parent_id: parent,
            slug: &slug,
            metadata: &metadata,
            markdown_content: &content,
            sibling_order: order,
        })
        .expect("hash");
        Node::new(
            NodeFields {
                id: node_id,
                slug,
                metadata,
                markdown_content: content,
                sibling_order: order,
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
        let f = node.fields();
        NodeRevision {
            node_id: node.id(),
            parent_id: node.parent_id(),
            slug: f.slug.clone(),
            metadata: f.metadata.clone(),
            markdown_content: f.markdown_content.clone(),
            sibling_order: f.sibling_order,
            version: 1,
            content_hash: f.content_hash,
            revision_hash: f.revision_hash,
            change_summary: None,
            created_by: None,
            created_at: 1,
        }
    }
    fn create(store: &mut SqliteStore, node: &Node, seed: u64) {
        let derived =
            build_derived_records(node, &SequentialUlidGenerator::new(seed)).expect("derived");
        store
            .create_node(node, &revision(node), &derived)
            .expect("create");
    }

    fn inspection_fixture() -> (tempfile::TempDir, SqliteStore, Node, Node, Node, Node) {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("context.mdtree");
        let root = node("01JZ8Q5CWPN8T7KPN5A1V9B6XM", None, "Project", "project", 0);
        let mut store =
            SqliteStore::new(create_workspace(&path, "Project", &root).expect("workspace"));
        let parent = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Parent",
            "parent",
            0,
        );
        create(&mut store, &parent, 100);
        let target = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(parent.id()),
            "Target",
            "target",
            0,
        );
        create(&mut store, &target, 200);
        let sibling = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XQ",
            Some(parent.id()),
            "Sibling",
            "sibling",
            1,
        );
        create(&mut store, &sibling, 300);
        let child = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XR",
            Some(target.id()),
            "Child",
            "child",
            0,
        );
        create(&mut store, &child, 400);
        (directory, store, root, parent, target, sibling)
    }

    #[test]
    fn inspection_continuation_preserves_depth_counts_and_breadcrumbs() {
        let (_directory, store, root, parent, target, sibling) = inspection_fixture();
        let first_page = store
            .inspect_subtree_page(
                "inspect",
                root.id(),
                2,
                PageLimit::new(2).expect("limit"),
                None,
            )
            .expect("first inspection page");
        assert_eq!(
            first_page
                .items
                .iter()
                .map(|item| item.node.node_id)
                .collect::<Vec<_>>(),
            [root.id(), parent.id()]
        );
        let cursor = first_page.next_cursor.expect("continuation");
        let second_page = store
            .inspect_subtree_page(
                "inspect",
                root.id(),
                2,
                PageLimit::new(2).expect("limit"),
                Some(&cursor),
            )
            .expect("second inspection page");
        assert_eq!(
            second_page
                .items
                .iter()
                .map(|item| item.node.node_id)
                .collect::<Vec<_>>(),
            [target.id(), sibling.id()]
        );
        assert_eq!(
            second_page.items[0].node.breadcrumb.segments(),
            ["Project", "Parent", "Target"]
        );
        assert!(second_page.next_cursor.is_none());
        assert!(second_page.truncated, "depth two omits Target's child");
    }

    #[test]
    fn inspection_and_context_limits_are_exact_and_priority_ordered() {
        let (_directory, store, root, _parent, target, _sibling) = inspection_fixture();
        store.connection().execute("INSERT INTO \"references\" (source_node_id,target_ref,reference_type,origin) VALUES (?1,'Missing','related_to','agent')",[target.id().to_string()]).expect("reference");

        let inspection = store
            .inspect_subtree_page(
                "inspect",
                root.id(),
                1,
                PageLimit::new(2).expect("limit"),
                None,
            )
            .expect("inspection");
        assert_eq!(inspection.items.len(), 2);
        assert!(inspection.items.iter().all(|item| item.depth <= 1));
        assert!(inspection.truncated);
        assert!(inspection.next_cursor.is_none());

        let full = store
            .read_context(target.id(), 20_000)
            .expect("read context");
        assert_eq!(full.ancestors.len(), 2);
        assert_eq!(full.children.len(), 1);
        assert_eq!(full.references.len(), 1);
        let full_size = serde_json::to_vec(&full).expect("JSON").len();
        let limited = store
            .read_context(target.id(), full_size - 1)
            .expect("limited context");
        assert!(serde_json::to_vec(&limited).expect("JSON").len() < full_size);
        assert!(limited.truncated);
        assert!(limited.omitted.contains(&"references".into()));
        assert_eq!(limited.children.len(), 1);
        assert!(matches!(
            store.read_context(target.id(), 1),
            Err(StoreError::BudgetExceeded { .. })
        ));

        let write = store
            .write_context(target.id(), 20_000)
            .expect("write context");
        assert_eq!(write.version, 1);
        assert_eq!(write.content_hash, target.fields().content_hash);
        assert_eq!(write.owns, ["database definitions"]);
        assert_eq!(write.sibling_examples.len(), 1);
        assert_eq!(write.ancestor_conventions.len(), 2);
        assert_eq!(write.references.len(), 1);
        let write_size = serde_json::to_vec(&write).expect("JSON").len();
        let limited_write = store
            .write_context(target.id(), write_size - 1)
            .expect("limited write");
        assert!(serde_json::to_vec(&limited_write).expect("JSON").len() < write_size);
        assert!(limited_write.omitted.contains(&"references".into()));
    }
}
