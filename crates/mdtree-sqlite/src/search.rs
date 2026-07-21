//! Section content search and structural destination location.

#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

use crate::{SqliteStore, StoreError};
use mdtree_core::{
    normalize_fts_query, CursorScope, DestinationCandidate, LocateAction, LocateResult,
    LocateStatus, Node, NodeId, NodeType, Page, PageCursor, PageLimit, PagePosition,
    ReferenceTarget, SearchMatch, SearchRequest, SearchScope,
};
use rusqlite::params_from_iter;

use crate::PageReadError;

impl SqliteStore {
    /// Executes weighted, section-oriented content search within a structural scope.
    #[allow(clippy::too_many_lines)]
    pub fn search_content(&self, request: &SearchRequest) -> Result<Vec<SearchMatch>, StoreError> {
        request
            .filters
            .validate(request.scope)
            .map_err(|message| StoreError::InvalidData(message.into()))?;
        let Some(query) = normalize_fts_query(&request.query, request.prefix_last_token) else {
            return Ok(Vec::new());
        };
        let eligible = self.eligible_nodes(request.scope, request.scope_node)?;
        let depths = if request.filters.min_depth.is_some() || request.filters.max_depth.is_some() {
            Some(self.relative_depths(request.scope, request.scope_node)?)
        } else {
            None
        };
        let status_nodes = self.status_nodes(&request.filters.statuses)?;
        let structural_nodes = self.structural_nodes(request.filters.structure)?;
        let tokens = tokens(&request.query);
        let mut statement = self.connection().prepare(
            "SELECT section_id,node_id,heading,content,title,aliases,summary,tags,keywords,
             ancestor_context,child_context FROM section_fts WHERE section_fts MATCH ?1",
        )?;
        let rows = statement.query_map([query], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
            ))
        })?;
        let mut best: HashMap<NodeId, (NodeId, f64, Vec<String>)> = HashMap::new();
        for row in rows {
            let (
                section,
                node,
                heading,
                content,
                title,
                aliases,
                summary,
                tags,
                keywords,
                ancestors,
                children,
            ) = row?;
            let node_id =
                NodeId::from_str(&node).map_err(|e| StoreError::InvalidData(e.to_string()))?;
            if !eligible.contains(&node_id) {
                continue;
            }
            let Some(node) = self.get(node_id)? else {
                continue;
            };
            if !matches_filters(
                &node,
                request,
                depths.as_ref(),
                status_nodes.as_ref(),
                structural_nodes.as_ref(),
            ) {
                continue;
            }
            let mut score = 0.0;
            let mut reasons = Vec::new();
            add_signal(&tokens, &title, 0.55, "title", &mut score, &mut reasons);
            add_signal(&tokens, &aliases, 0.48, "alias", &mut score, &mut reasons);
            add_signal(&tokens, &summary, 0.34, "summary", &mut score, &mut reasons);
            add_signal(
                &tokens,
                &keywords,
                0.30,
                "keyword",
                &mut score,
                &mut reasons,
            );
            add_signal(&tokens, &tags, 0.24, "tag", &mut score, &mut reasons);
            add_signal(
                &tokens,
                &ancestors,
                0.24,
                "ancestor context",
                &mut score,
                &mut reasons,
            );
            add_signal(
                &tokens,
                &children,
                0.20,
                "child context",
                &mut score,
                &mut reasons,
            );
            add_signal(&tokens, &heading, 0.18, "heading", &mut score, &mut reasons);
            add_signal(&tokens, &content, 0.08, "content", &mut score, &mut reasons);
            let section_id =
                NodeId::from_str(&section).map_err(|e| StoreError::InvalidData(e.to_string()))?;
            let entry = best
                .entry(node_id)
                .or_insert((section_id, score, reasons.clone()));
            if score.total_cmp(&entry.1).is_gt()
                || (score.total_cmp(&entry.1).is_eq() && section_id < entry.0)
            {
                *entry = (section_id, score, reasons);
            }
        }
        let mut results = Vec::new();
        for (node_id, (section_id, score, reasons)) in best {
            results.push(self.search_match(
                node_id,
                Some(section_id),
                score.min(1.0),
                reasons,
                None,
            )?);
        }
        results.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.node_id.cmp(&right.node_id))
        });
        let offset = usize::try_from(request.offset)
            .map_err(|_| StoreError::InvalidData("offset".into()))?;
        let limit =
            usize::try_from(request.limit).map_err(|_| StoreError::InvalidData("limit".into()))?;
        Ok(results.into_iter().skip(offset).take(limit).collect())
    }

    /// Returns one resumable page of deterministically ranked content-search matches.
    pub fn search_content_page(
        &self,
        request: &SearchRequest,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<SearchMatch>, PageReadError> {
        let revision = self.workspace_revision()?;
        let request_key = serde_json::to_string(&(
            &request.query,
            request.scope,
            &request.scope_node,
            &request.filters,
            request.prefix_last_token,
        ))
        .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let scope = CursorScope::new("search", request.scope_node, &request_key)?;
        let offset = match cursor
            .map(|value| value.resume(&scope, revision))
            .transpose()?
        {
            None => 0,
            Some(PagePosition::Search { offset }) => offset,
            Some(_) => return Err(mdtree_core::PaginationError::InvalidCursorPosition.into()),
        };

        let mut page_request = request.clone();
        page_request.offset = offset;
        page_request.limit = limit.get().saturating_add(1);
        let mut items = self.search_content(&page_request)?;
        let has_more = items.len() > usize::try_from(limit.get()).unwrap_or(usize::MAX);
        if has_more {
            items.pop();
        }
        let next_cursor = if has_more {
            let returned = u32::try_from(items.len())
                .map_err(|_| StoreError::InvalidData("search page length".into()))?;
            Some(PageCursor::issue(
                revision,
                scope,
                PagePosition::Search {
                    offset: offset.saturating_add(returned),
                },
            )?)
        } else {
            None
        };
        Ok(Page::new(items, next_cursor))
    }

    /// Ranks structural destinations using ownership and local tree conventions.
    #[allow(clippy::too_many_lines)]
    pub fn destination_candidates(
        &self,
        query: &str,
        proposed_type: Option<&NodeType>,
        limit: u32,
    ) -> Result<Vec<DestinationCandidate>, StoreError> {
        let tokens = tokens(query);
        let inferred = infer_type(query);
        let requested = proposed_type.or(inferred.as_ref());
        let mut candidates = Vec::new();
        for node_id in self.all_node_ids()? {
            let node = self
                .get(node_id)?
                .ok_or_else(|| StoreError::NotFound(node_id.to_string()))?;
            let fields = node.fields();
            let children = self.children(node_id)?;
            let ancestor_text = self.breadcrumb(node_id)?.to_string();
            let child_text = children
                .iter()
                .map(|child| child.fields().metadata.title.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let mut score = 0.0;
            let mut reasons = Vec::new();
            add_signal(
                &tokens,
                &fields.metadata.title,
                0.20,
                "title",
                &mut score,
                &mut reasons,
            );
            add_signal(
                &tokens,
                &fields.metadata.aliases.join(" "),
                0.18,
                "alias",
                &mut score,
                &mut reasons,
            );
            add_signal(
                &tokens,
                fields.metadata.summary.as_deref().unwrap_or(""),
                0.15,
                "summary",
                &mut score,
                &mut reasons,
            );
            add_signal(
                &tokens,
                &fields.metadata.owns.join(" "),
                0.45,
                "ownership",
                &mut score,
                &mut reasons,
            );
            add_signal(
                &tokens,
                &ancestor_text,
                0.12,
                "ancestor path",
                &mut score,
                &mut reasons,
            );
            add_signal(
                &tokens,
                &child_text,
                0.12,
                "existing child pattern",
                &mut score,
                &mut reasons,
            );
            if fields
                .metadata
                .excludes
                .iter()
                .any(|value| overlap(&tokens, value) > 0.0)
            {
                score -= 0.35;
                reasons.push("exclusion penalty".into());
            }
            let accepts = requested.map(|kind| fields.metadata.accepts_children.contains(kind));
            if let (Some(kind), Some(true)) = (requested, accepts) {
                score += 0.38;
                reasons.push(format!("accepts {kind} children"));
            }
            if let Some(kind) = requested {
                let matching = children
                    .iter()
                    .filter(|child| child.fields().metadata.node_type.as_ref() == Some(kind))
                    .count();
                if matching > 0 {
                    score += 0.18;
                    reasons.push(format!("contains {matching} matching child examples"));
                }
            }
            if fields.metadata.accepts_children.is_empty() && children.is_empty() {
                score -= 0.08;
            }
            if score <= 0.0 {
                continue;
            }
            let examples = self.examples_for(node_id, requested, 5)?;
            candidates.push(DestinationCandidate {
                result: self.search_match(
                    node_id,
                    None,
                    score.clamp(0.0, 1.0),
                    reasons,
                    accepts,
                )?,
                example_node_ids: examples,
            });
        }
        candidates.sort_by(|left, right| {
            right
                .result
                .score
                .total_cmp(&left.result.score)
                .then_with(|| left.result.node_id.cmp(&right.result.node_id))
        });
        candidates.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(candidates)
    }

    /// Finds nearby examples by requested node type in deterministic sibling order.
    pub fn examples_for(
        &self,
        destination: NodeId,
        node_type: Option<&NodeType>,
        limit: u32,
    ) -> Result<Vec<NodeId>, StoreError> {
        let mut examples: Vec<_> = self
            .children(destination)?
            .into_iter()
            .filter(|node| {
                node_type.is_none_or(|kind| node.fields().metadata.node_type.as_ref() == Some(kind))
            })
            .map(|node| node.id())
            .collect();
        examples.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(examples)
    }

    /// Combines destination ranking, ambiguity, action, title, and examples.
    pub fn locate_target(
        &self,
        query: &str,
        proposed_type: Option<&NodeType>,
    ) -> Result<LocateResult, StoreError> {
        let candidates = self.destination_candidates(query, proposed_type, 5)?;
        if candidates.is_empty() {
            return Ok(LocateResult {
                status: LocateStatus::NotFound,
                action: None,
                candidates,
                suggested_title: None,
                ambiguity: None,
            });
        }
        let ambiguous = candidates
            .get(1)
            .is_some_and(|second| candidates[0].result.score - second.result.score < 0.08);
        Ok(LocateResult {
            status: if ambiguous {
                LocateStatus::Ambiguous
            } else {
                LocateStatus::Recommended
            },
            action: Some(if candidates[0].result.accepts_child == Some(true) {
                LocateAction::CreateChild
            } else {
                LocateAction::AppendToNode
            }),
            suggested_title: suggested_title(query),
            ambiguity: ambiguous
                .then(|| "Top structural destinations have near-equal scores".into()),
            candidates,
        })
    }

    fn eligible_nodes(
        &self,
        scope: SearchScope,
        node: Option<NodeId>,
    ) -> Result<HashSet<NodeId>, StoreError> {
        let required =
            || node.ok_or_else(|| StoreError::InvalidData("scope_node is required".into()));
        let nodes = match scope {
            SearchScope::Workspace => self.all_node_ids()?,
            SearchScope::CurrentNode => vec![required()?],
            SearchScope::Subtree => self
                .subtree(required()?)?
                .into_iter()
                .map(|item| item.node.id())
                .collect(),
            SearchScope::Siblings => self
                .siblings(required()?)?
                .into_iter()
                .map(|node| node.id())
                .collect(),
            SearchScope::ParentSubtree => {
                let id = required()?;
                let root = self.parent(id)?.map_or(id, |parent| parent.id());
                self.subtree(root)?
                    .into_iter()
                    .map(|item| item.node.id())
                    .collect()
            }
            SearchScope::Linked => {
                let id = required()?;
                let mut linked = vec![id];
                for reference in self.outgoing_references(id)? {
                    if let ReferenceTarget::Resolved { node_id, .. } = reference.target {
                        linked.push(node_id);
                    }
                }
                for reference in self.backlinks(id)? {
                    linked.push(reference.source_node_id);
                }
                linked
            }
        };
        Ok(nodes.into_iter().collect())
    }

    fn relative_depths(
        &self,
        scope: SearchScope,
        node: Option<NodeId>,
    ) -> Result<HashMap<NodeId, u32>, StoreError> {
        let required =
            || node.ok_or_else(|| StoreError::InvalidData("scope_node is required".into()));
        let anchor = match scope {
            SearchScope::Workspace => self.root()?.id(),
            SearchScope::CurrentNode | SearchScope::Subtree => required()?,
            SearchScope::Siblings | SearchScope::ParentSubtree => {
                let selected = required()?;
                self.parent(selected)?
                    .map_or(selected, |parent| parent.id())
            }
            SearchScope::Linked => {
                return Err(StoreError::InvalidData(
                    "depth filters are not supported for linked scope".into(),
                ));
            }
        };
        let mut statement = self.connection().prepare(
            "WITH RECURSIVE x(id,depth) AS (
             SELECT id,0 FROM nodes WHERE id=?1
             UNION ALL SELECT n.id,x.depth+1 FROM nodes n JOIN x ON n.parent_id=x.id
             ) SELECT id,depth FROM x",
        )?;
        let rows = statement.query_map([anchor.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })?;
        rows.map(|row| {
            let (id, depth) = row?;
            Ok((
                NodeId::from_str(&id)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                depth,
            ))
        })
        .collect()
    }

    fn status_nodes(
        &self,
        statuses: &[mdtree_core::ReferenceType],
    ) -> Result<Option<HashSet<NodeId>>, StoreError> {
        if statuses.is_empty() {
            return Ok(None);
        }
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = self.connection().prepare(&format!(
            "SELECT DISTINCT source_node_id FROM \"references\" WHERE reference_type IN ({placeholders})"
        ))?;
        let rows = statement.query_map(
            params_from_iter(statuses.iter().map(ToString::to_string)),
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            NodeId::from_str(&row?).map_err(|error| StoreError::InvalidData(error.to_string()))
        })
        .collect::<Result<HashSet<_>, _>>()
        .map(Some)
    }

    fn structural_nodes(
        &self,
        predicate: Option<mdtree_core::StructuralPredicate>,
    ) -> Result<Option<HashSet<NodeId>>, StoreError> {
        let Some(predicate) = predicate else {
            return Ok(None);
        };
        let expect_children = predicate == mdtree_core::StructuralPredicate::Internal;
        let mut statement = self.connection().prepare(
            "SELECT n.id FROM nodes n
             WHERE EXISTS(SELECT 1 FROM nodes child WHERE child.parent_id=n.id)=?1",
        )?;
        let rows = statement.query_map([expect_children], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            NodeId::from_str(&row?).map_err(|error| StoreError::InvalidData(error.to_string()))
        })
        .collect::<Result<HashSet<_>, _>>()
        .map(Some)
    }

    fn all_node_ids(&self) -> Result<Vec<NodeId>, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT id FROM nodes ORDER BY id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| NodeId::from_str(&row?).map_err(|e| StoreError::InvalidData(e.to_string())))
            .collect()
    }

    fn search_match(
        &self,
        node_id: NodeId,
        section_id: Option<NodeId>,
        score: f64,
        reasons: Vec<String>,
        accepts: Option<bool>,
    ) -> Result<SearchMatch, StoreError> {
        let node = self
            .get(node_id)?
            .ok_or_else(|| StoreError::NotFound(node_id.to_string()))?;
        Ok(SearchMatch {
            node_id,
            section_id,
            breadcrumb: self.breadcrumb(node_id)?,
            title: node.fields().metadata.title.clone(),
            summary: node.fields().metadata.summary.clone(),
            node_type: node.fields().metadata.node_type.clone(),
            score,
            match_reasons: reasons,
            child_count: u32::try_from(self.children(node_id)?.len())
                .map_err(|_| StoreError::InvalidData("child count".into()))?,
            accepts_child: accepts,
        })
    }
}

fn tokens(query: &str) -> BTreeSet<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|v| !v.is_empty())
        .map(str::to_lowercase)
        .collect()
}
fn overlap(tokens: &BTreeSet<String>, text: &str) -> f64 {
    let lower = text.to_lowercase();
    let matched = tokens
        .iter()
        .filter(|token| lower.contains(token.as_str()))
        .count();
    if tokens.is_empty() {
        0.0
    } else {
        let matched = u32::try_from(matched).unwrap_or(u32::MAX);
        let total = u32::try_from(tokens.len()).unwrap_or(u32::MAX);
        f64::from(matched) / f64::from(total)
    }
}
fn add_signal(
    tokens: &BTreeSet<String>,
    text: &str,
    weight: f64,
    label: &str,
    score: &mut f64,
    reasons: &mut Vec<String>,
) {
    let value = overlap(tokens, text);
    if value > 0.0 {
        *score += weight * value;
        reasons.push(format!("{label} matched"));
    }
}
fn matches_filters(
    node: &Node,
    request: &SearchRequest,
    depths: Option<&HashMap<NodeId, u32>>,
    status_nodes: Option<&HashSet<NodeId>>,
    structural_nodes: Option<&HashSet<NodeId>>,
) -> bool {
    let metadata = &node.fields().metadata;
    (request.filters.node_types.is_empty()
        || metadata
            .node_type
            .as_ref()
            .is_some_and(|kind| request.filters.node_types.contains(kind)))
        && (request.filters.tags.is_empty()
            || request
                .filters
                .tags
                .iter()
                .all(|tag| metadata.tags.contains(tag)))
        && status_nodes.is_none_or(|nodes| nodes.contains(&node.id()))
        && structural_nodes.is_none_or(|nodes| nodes.contains(&node.id()))
        && depths.is_none_or(|values| {
            values.get(&node.id()).is_some_and(|depth| {
                request
                    .filters
                    .min_depth
                    .is_none_or(|minimum| *depth >= minimum)
                    && request
                        .filters
                        .max_depth
                        .is_none_or(|maximum| *depth <= maximum)
            })
        })
        && request
            .filters
            .created_from
            .is_none_or(|minimum| node.fields().created_at >= minimum)
        && request
            .filters
            .created_to
            .is_none_or(|maximum| node.fields().created_at <= maximum)
        && request
            .filters
            .updated_from
            .is_none_or(|minimum| node.fields().updated_at >= minimum)
        && request
            .filters
            .updated_to
            .is_none_or(|maximum| node.fields().updated_at <= maximum)
}
fn infer_type(query: &str) -> Option<NodeType> {
    let lower = query.to_lowercase();
    let value = if lower.contains("table") {
        "database_table"
    } else if lower.contains("endpoint") {
        "api_endpoint"
    } else if lower.contains("model") {
        "database_model"
    } else {
        return None;
    };
    NodeType::from_str(value).ok()
}
fn suggested_title(query: &str) -> Option<String> {
    let ignored = [
        "add",
        "create",
        "append",
        "new",
        "table",
        "model",
        "endpoint",
        "definition",
    ];
    let words: Vec<_> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty() && !ignored.contains(&word.to_lowercase().as_str()))
        .collect();
    (!words.is_empty()).then(|| {
        words
            .into_iter()
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join(" ")
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        generate_large_tree_fixture, hash_content, hash_revision, LargeTreeFixtureSpec,
        LocateAction, LocateStatus, Node, NodeFields, NodeId, NodeMetadata, NodeRevision, NodeType,
        PageLimit, PaginationErrorCode, ReferenceType, RevisionHashInput, SearchFilters,
        SearchRequest, SearchScope, SequentialUlidGenerator, Slug, StructuralPredicate,
    };
    use mdtree_markdown::build_derived_records;
    use tempfile::{tempdir, TempDir};

    use crate::{create_workspace, SqliteStore};

    struct SearchFixture {
        _dir: TempDir,
        store: SqliteStore,
        tables: NodeId,
        orders: NodeId,
        create_order: NodeId,
        endpoints: NodeId,
    }
    fn id(value: &str) -> NodeId {
        NodeId::from_str(value).expect("ID")
    }
    fn kind(value: &str) -> NodeType {
        NodeType::from_str(value).expect("type")
    }
    fn meta(title: &str, ty: &str) -> NodeMetadata {
        let mut value = NodeMetadata::new(title);
        value.node_type = Some(kind(ty));
        value
    }
    fn node(
        raw: &str,
        parent: Option<NodeId>,
        slug: &str,
        metadata: NodeMetadata,
        content: &str,
        order: u32,
    ) -> Node {
        let node_id = id(raw);
        let slug = Slug::from_str(slug).expect("slug");
        let content_hash = hash_content(content);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id,
            parent_id: parent,
            slug: &slug,
            metadata: &metadata,
            markdown_content: content,
            sibling_order: order,
        })
        .expect("hash");
        Node::new(
            NodeFields {
                id: node_id,
                slug,
                metadata,
                markdown_content: content.into(),
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
            change_summary: Some("seed".into()),
            created_by: Some("test".into()),
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
    #[allow(clippy::too_many_lines)]
    fn northstar_fixture() -> SearchFixture {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("northstar.mdtree");
        let root = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XM",
            None,
            "northstar-platform",
            meta("Northstar Platform", "project"),
            "# Northstar Platform\n",
            0,
        );
        let mut store = SqliteStore::new(
            create_workspace(&path, "Northstar Platform", &root).expect("workspace"),
        );
        let database = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "database",
            meta("Database", "area"),
            "# Database\n",
            0,
        );
        create(&mut store, &database, 100);
        let mut tables_meta = meta("Tables", "collection");
        tables_meta.owns = vec![
            "database table definitions".into(),
            "column definitions".into(),
        ];
        tables_meta.accepts_children = vec![kind("database_table")];
        tables_meta.excludes = vec!["api contracts".into()];
        let tables = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(database.id()),
            "tables",
            tables_meta,
            "# Tables\nCanonical database tables.\n",
            0,
        );
        create(&mut store, &tables, 200);
        for (raw, slug, title, content, order, seed) in [
            (
                "01JZ8Q5CWPN8T7KPN5A1V9B6XQ",
                "products",
                "Products",
                "# Products\nProduct table model.\n",
                0,
                300,
            ),
            (
                "01JZ8Q5CWPN8T7KPN5A1V9B6XR",
                "customers",
                "Customers",
                "# Customers\nCustomer table model.\n",
                1,
                400,
            ),
            (
                "01JZ8Q5CWPN8T7KPN5A1V9B6XS",
                "orders",
                "Orders",
                "# Orders\nCanonical order model and table.\n",
                2,
                500,
            ),
        ] {
            let mut metadata = meta(title, "database_table");
            if slug == "orders" {
                metadata.tags = vec!["database".into(), "orders".into()];
            }
            let item = node(raw, Some(tables.id()), slug, metadata, content, order);
            create(&mut store, &item, seed);
        }
        let orders = id("01JZ8Q5CWPN8T7KPN5A1V9B6XS");
        let api = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XT",
            Some(root.id()),
            "api",
            meta("API", "area"),
            "# API\n",
            1,
        );
        create(&mut store, &api, 600);
        let mut endpoints_meta = meta("Endpoints", "collection");
        endpoints_meta.owns = vec!["http endpoint definitions".into()];
        endpoints_meta.accepts_children = vec![kind("api_endpoint")];
        let endpoints = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XV",
            Some(api.id()),
            "endpoints",
            endpoints_meta,
            "# Endpoints\n",
            0,
        );
        create(&mut store, &endpoints, 700);
        let mut create_order_meta = meta("Create Order", "api_endpoint");
        create_order_meta.tags = vec!["api".into(), "orders".into()];
        let create_order = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XW",
            Some(endpoints.id()),
            "create-order",
            create_order_meta,
            "# Create Order\nOrder model endpoint.\n",
            0,
        );
        create(&mut store, &create_order, 800);
        let mut get_order_meta = meta("Get Order", "api_endpoint");
        get_order_meta.tags = vec!["api".into(), "orders".into()];
        let get_order = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XX",
            Some(endpoints.id()),
            "get-order",
            get_order_meta,
            "# Get Order\nRead order model endpoint.\n",
            1,
        );
        create(&mut store, &get_order, 900);
        store.connection().execute("INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin) VALUES (?1,?2,'Orders','reads_from','explicit')",rusqlite::params![create_order.id().to_string(),orders.to_string()]).expect("link");
        SearchFixture {
            _dir: dir,
            store,
            tables: tables.id(),
            orders,
            create_order: create_order.id(),
            endpoints: endpoints.id(),
        }
    }
    fn request(query: &str, scope: SearchScope, node: Option<NodeId>) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            scope,
            scope_node: node,
            filters: SearchFilters::default(),
            limit: 20,
            offset: 0,
            prefix_last_token: false,
        }
    }

    #[test]
    fn weighted_search_and_scopes_are_structural() {
        let candy = northstar_fixture();
        let workspace = candy
            .store
            .search_content(&request("order model", SearchScope::Workspace, None))
            .expect("search");
        assert_eq!(workspace[0].node_id, candy.orders);
        for (scope, node, allowed) in [
            (SearchScope::CurrentNode, candy.orders, vec![candy.orders]),
            (SearchScope::Subtree, candy.tables, vec![candy.orders]),
            (SearchScope::Siblings, candy.orders, vec![candy.orders]),
            (SearchScope::ParentSubtree, candy.orders, vec![candy.orders]),
            (
                SearchScope::Linked,
                candy.create_order,
                vec![candy.orders, candy.create_order],
            ),
        ] {
            let results = candy
                .store
                .search_content(&request("order", scope, Some(node)))
                .expect("scope");
            assert!(results
                .iter()
                .all(|result| allowed.contains(&result.node_id)));
        }
        let ancestor = candy
            .store
            .search_content(&request("database products", SearchScope::Workspace, None))
            .expect("ancestor");
        assert_eq!(ancestor[0].title, "Products");
    }

    #[test]
    fn node_type_and_required_tag_filters_apply_before_pagination() {
        let fixture = northstar_fixture();
        let mut filtered = request("order model", SearchScope::Workspace, None);
        filtered.filters.node_types = vec![kind("database_table")];
        filtered.filters.tags = vec!["database".into()];
        let matches = fixture
            .store
            .search_content(&filtered)
            .expect("filtered search");
        assert_eq!(
            matches.iter().map(|item| item.node_id).collect::<Vec<_>>(),
            vec![fixture.orders]
        );

        filtered.filters.node_types = vec![kind("database_table"), kind("api_endpoint")];
        filtered.filters.tags = vec!["orders".into()];
        filtered.limit = 1;
        let first = fixture
            .store
            .search_content_page(&filtered, PageLimit::new(1).expect("limit"), None)
            .expect("first filtered page");
        assert_eq!(first.items.len(), 1);
        let second = fixture
            .store
            .search_content_page(
                &filtered,
                PageLimit::new(1).expect("limit"),
                first.next_cursor.as_ref(),
            )
            .expect("second filtered page");
        assert_eq!(second.items.len(), 1);
        assert_ne!(first.items[0].node_id, second.items[0].node_id);
        assert!(second.next_cursor.is_some());

        let mut structural = request("order model", SearchScope::Subtree, Some(fixture.tables));
        structural.filters.min_depth = Some(1);
        structural.filters.max_depth = Some(1);
        structural.filters.created_from = Some(1);
        structural.filters.created_to = Some(1);
        structural.filters.updated_from = Some(1);
        structural.filters.updated_to = Some(1);
        structural.filters.structure = Some(StructuralPredicate::Leaf);
        let matches = fixture
            .store
            .search_content(&structural)
            .expect("structural temporal search");
        assert_eq!(
            matches.iter().map(|item| item.node_id).collect::<Vec<_>>(),
            vec![fixture.orders]
        );

        let mut status = request("order model", SearchScope::Workspace, None);
        status.filters.statuses = vec![ReferenceType::from_str("reads_from").expect("status")];
        let matches = fixture
            .store
            .search_content(&status)
            .expect("status search");
        assert_eq!(
            matches.iter().map(|item| item.node_id).collect::<Vec<_>>(),
            vec![fixture.create_order]
        );
    }

    #[test]
    fn paged_search_enumerates_large_tied_rankings_and_binds_the_request() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("search-pages.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 230,
                deep_descendants: 0,
                history_revisions: 1,
                relations: 0,
                response_boundary_bytes: 256,
            },
            911,
        );
        crate::import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let store = SqliteStore::open(&path).expect("store");
        for child in &fixture.wide_child_ids {
            store
                .connection()
                .execute(
                    "INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin) VALUES (?1,?2,?2,'search_scope','explicit')",
                    rusqlite::params![fixture.wide_parent_id.to_string(), child.to_string()],
                )
                .expect("reference");
        }

        let expected = {
            let mut ids = fixture.wide_child_ids.clone();
            ids.sort_unstable();
            ids
        };
        for (scope, scope_node) in [
            (SearchScope::Workspace, None),
            (SearchScope::Subtree, Some(fixture.wide_parent_id)),
            (SearchScope::Siblings, Some(fixture.wide_child_ids[0])),
            (SearchScope::ParentSubtree, Some(fixture.wide_child_ids[0])),
            (SearchScope::Linked, Some(fixture.wide_parent_id)),
        ] {
            let request = request("deterministic wide fixture", scope, scope_node);
            let mut cursor = None;
            let mut ids = Vec::new();
            loop {
                let page = store
                    .search_content_page(
                        &request,
                        PageLimit::new(37).expect("limit"),
                        cursor.as_ref(),
                    )
                    .expect("search page");
                ids.extend(page.items.into_iter().map(|item| item.node_id));
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(ids, expected, "{scope:?}");
        }

        let original = request(
            "deterministic wide fixture",
            SearchScope::Subtree,
            Some(fixture.wide_parent_id),
        );
        let first = store
            .search_content_page(&original, PageLimit::new(2).expect("limit"), None)
            .expect("first page");
        let changed = request("different query", original.scope, original.scope_node);
        let error = store
            .search_content_page(
                &changed,
                PageLimit::new(2).expect("limit"),
                first.next_cursor.as_ref(),
            )
            .expect_err("changed query must reject continuation");
        assert_eq!(
            error.pagination_code(),
            Some(PaginationErrorCode::InvalidCursor)
        );
    }

    #[test]
    fn locate_refunds_returns_tables_action_reasons_and_examples() {
        let candy = northstar_fixture();
        let ty = kind("database_table");
        let result = candy
            .store
            .locate_target("Add refunds table", Some(&ty))
            .expect("locate");
        assert_eq!(result.status, LocateStatus::Recommended);
        assert_eq!(result.action, Some(LocateAction::CreateChild));
        assert_eq!(result.candidates[0].result.node_id, candy.tables);
        assert_eq!(result.suggested_title.as_deref(), Some("Refunds"));
        assert!(result.candidates[0]
            .result
            .match_reasons
            .iter()
            .any(|reason| reason.contains("ownership")));
        assert_eq!(result.candidates[0].example_node_ids.len(), 3);
        assert_eq!(
            candy
                .store
                .examples_for(candy.endpoints, Some(&kind("api_endpoint")), 10)
                .expect("examples")[0],
            candy.create_order
        );
    }

    #[test]
    fn near_equal_destinations_report_ambiguity() {
        let mut candy = northstar_fixture();
        let root = candy.store.root().expect("root");
        let ty = kind("database_model");
        for (raw, slug, title, seed, order) in [
            (
                "01JZ8Q5CWPN8T7KPN5A1V9B6XY",
                "central-models",
                "Central Models",
                1100,
                2,
            ),
            (
                "01JZ8Q5CWPN8T7KPN5A1V9B6XZ",
                "billing-models",
                "Billing Models",
                1200,
                3,
            ),
        ] {
            let mut metadata = meta(title, "collection");
            metadata.owns = vec!["database model definitions".into()];
            metadata.accepts_children = vec![ty.clone()];
            let item = node(raw, Some(root.id()), slug, metadata, "# Models\n", order);
            create(&mut candy.store, &item, seed);
        }
        let result = candy
            .store
            .locate_target("Add invoice model", Some(&ty))
            .expect("locate");
        assert_eq!(result.status, LocateStatus::Ambiguous);
        assert!(result.candidates.len() >= 2);
        assert!(result.ambiguity.is_some());
    }
}
