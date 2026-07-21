//! Targeted canonical projections shared by CLI, MCP, and other adapters.

#![allow(clippy::missing_errors_doc)]

use std::str::FromStr;

use mdtree_core::{
    diff_subtrees, project_canonical_nodes, AdjacentSiblings, AncestorContainment,
    BatchChildrenLookupItem, BatchChildrenRequest, BatchLookupError, BatchNodeLookupItem,
    ContextSummary, CursorScope, IndexedChild, Node, NodeChildCount, NodeDepthProjection, NodeId,
    NodeProjection, NodeSelector, Page, PageCursor, PageLimit, PagePosition, PaginationError,
    PaginationErrorCode, PathBetween, RevisionSummary, StructuralPredicate, SubtreeDiffItem,
    TraversalOrder, TreeDistance, TreeStatistics, MAX_BATCH_CHILDREN, MAX_BATCH_ITEMS,
    MAX_BATCH_PARENTS,
};
use rusqlite::{params, params_from_iter};
use serde::Serialize;
use thiserror::Error;

use crate::store::{query_nodes, query_ordered_depths, NODE_COLUMNS, SELECT_NODE};
use crate::{NodeDepth, SqliteStore, StoreError};

/// Bounded read-only integrity result with a stable summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IntegrityPage {
    /// Whether the complete validation found no violations.
    pub healthy: bool,
    /// Total findings across all pages.
    pub total_findings: u64,
    /// Bounded stable page of actionable findings.
    #[serde(flatten)]
    pub page: Page<crate::IntegrityFinding>,
}

/// Failure from a bounded structural page read.
#[derive(Debug, Error)]
pub enum PageReadError {
    /// Canonical storage read failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Limit or continuation validation failed.
    #[error(transparent)]
    Pagination(#[from] PaginationError),
}

impl PageReadError {
    /// Returns the stable pagination category when this is a continuation failure.
    #[must_use]
    pub fn pagination_code(&self) -> Option<PaginationErrorCode> {
        match self {
            Self::Store(_) => None,
            Self::Pagination(error) => Some(error.code()),
        }
    }
}

impl SqliteStore {
    /// Resolves a bounded selector list in caller order with per-item errors.
    pub fn batch_node_lookup(
        &self,
        selectors: &[String],
    ) -> Result<Vec<BatchNodeLookupItem>, StoreError> {
        if selectors.is_empty() || selectors.len() > MAX_BATCH_ITEMS {
            return Err(StoreError::InvalidData(format!(
                "batch selector count {} is outside 1..={MAX_BATCH_ITEMS}",
                selectors.len()
            )));
        }
        selectors
            .iter()
            .map(|raw| {
                let selector = match NodeSelector::from_str(raw) {
                    Ok(selector) => selector,
                    Err(error) => {
                        return Ok(BatchNodeLookupItem {
                            selector: raw.clone(),
                            node: None,
                            error: Some(BatchLookupError {
                                code: "invalid_selector".into(),
                                message: error.to_string(),
                            }),
                        });
                    }
                };
                let node = self.resolve_projection(&selector)?;
                Ok(BatchNodeLookupItem {
                    selector: raw.clone(),
                    error: node.is_none().then(|| BatchLookupError {
                        code: "not_found".into(),
                        message: format!("node not found: {raw}"),
                    }),
                    node,
                })
            })
            .collect()
    }

    /// Returns grouped canonical child pages with aggregate request bounds.
    pub fn batch_children_lookup(
        &self,
        requests: &[BatchChildrenRequest],
    ) -> Result<Vec<BatchChildrenLookupItem>, StoreError> {
        if requests.is_empty() || requests.len() > MAX_BATCH_PARENTS {
            return Err(StoreError::InvalidData(format!(
                "batch parent count {} is outside 1..={MAX_BATCH_PARENTS}",
                requests.len()
            )));
        }
        let aggregate = requests.iter().try_fold(0_u32, |total, request| {
            total
                .checked_add(request.limit)
                .ok_or_else(|| StoreError::InvalidData("batch child limit overflow".into()))
        })?;
        if aggregate > MAX_BATCH_CHILDREN {
            return Err(StoreError::InvalidData(format!(
                "aggregate child limit {aggregate} exceeds {MAX_BATCH_CHILDREN}"
            )));
        }
        requests
            .iter()
            .map(|request| {
                let failure = |code: &str, message: String| BatchChildrenLookupItem {
                    parent: request.parent.clone(),
                    page: None,
                    error: Some(BatchLookupError {
                        code: code.into(),
                        message,
                    }),
                };
                let selector = match NodeSelector::from_str(&request.parent) {
                    Ok(selector) => selector,
                    Err(error) => return Ok(failure("invalid_selector", error.to_string())),
                };
                let Some(parent) = self.resolve(&selector)? else {
                    return Ok(failure(
                        "not_found",
                        format!("node not found: {}", request.parent),
                    ));
                };
                let limit = match PageLimit::new(request.limit) {
                    Ok(limit) => limit,
                    Err(error) => return Ok(failure("invalid_limit", error.to_string())),
                };
                let cursor = match request
                    .cursor
                    .as_deref()
                    .map(str::parse::<PageCursor>)
                    .transpose()
                {
                    Ok(cursor) => cursor,
                    Err(error) => return Ok(failure("invalid_cursor", error.to_string())),
                };
                match self.children_page(parent.id(), limit, cursor.as_ref()) {
                    Ok(page) => Ok(BatchChildrenLookupItem {
                        parent: request.parent.clone(),
                        page: Some(page),
                        error: None,
                    }),
                    Err(PageReadError::Pagination(error)) => Ok(failure(
                        match error.code() {
                            PaginationErrorCode::InvalidLimit => "invalid_limit",
                            PaginationErrorCode::InvalidCursor => "invalid_cursor",
                            PaginationErrorCode::StaleCursor => "stale_cursor",
                        },
                        error.to_string(),
                    )),
                    Err(PageReadError::Store(error)) => Err(error),
                }
            })
            .collect()
    }

    /// Returns one zero-based child directly through the canonical sibling index.
    pub fn child_at(&self, parent_id: NodeId, index: u32) -> Result<IndexedChild, StoreError> {
        if self.get(parent_id)?.is_none() {
            return Err(StoreError::NotFound(parent_id.to_string()));
        }
        let mut nodes = query_nodes(
            self.connection(),
            &format!(
                "{SELECT_NODE} WHERE n.parent_id=?1
                 ORDER BY n.sibling_order,n.id LIMIT 1 OFFSET ?2"
            ),
            params![parent_id.to_string(), i64::from(index)],
        )?;
        Ok(IndexedChild {
            parent_id,
            index,
            node: nodes.pop().as_ref().map(NodeProjection::from),
        })
    }

    /// Returns direct previous and next canonical siblings without loading the collection.
    pub fn adjacent_siblings(&self, id: NodeId) -> Result<AdjacentSiblings, StoreError> {
        let selected = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let Some(parent_id) = selected.parent_id() else {
            return Ok(AdjacentSiblings {
                node_id: id,
                previous: None,
                next: None,
            });
        };
        let order = selected.fields().sibling_order;
        let previous = query_nodes(
            self.connection(),
            &format!(
                "{SELECT_NODE} WHERE n.parent_id=?1 AND
                 (n.sibling_order<?2 OR (n.sibling_order=?2 AND n.id<?3))
                 ORDER BY n.sibling_order DESC,n.id DESC LIMIT 1"
            ),
            params![parent_id.to_string(), order, id.to_string()],
        )?
        .pop()
        .as_ref()
        .map(NodeProjection::from);
        let next = query_nodes(
            self.connection(),
            &format!(
                "{SELECT_NODE} WHERE n.parent_id=?1 AND
                 (n.sibling_order>?2 OR (n.sibling_order=?2 AND n.id>?3))
                 ORDER BY n.sibling_order,n.id LIMIT 1"
            ),
            params![parent_id.to_string(), order, id.to_string()],
        )?
        .pop()
        .as_ref()
        .map(NodeProjection::from);
        Ok(AdjacentSiblings {
            node_id: id,
            previous,
            next,
        })
    }

    /// Returns compact edge distance and LCA for two canonical nodes.
    pub fn tree_distance(
        &self,
        from_id: NodeId,
        to_id: NodeId,
    ) -> Result<TreeDistance, StoreError> {
        let (lca, from_distance, to_distance) = relationship_distances(self, from_id, to_id)?;
        Ok(TreeDistance {
            from_id,
            to_id,
            lowest_common_ancestor_id: lca,
            distance: from_distance
                .checked_add(to_distance)
                .ok_or_else(|| StoreError::InvalidData("tree distance overflow".into()))?,
        })
    }

    /// Returns the endpoint-inclusive canonical route from `from_id` to `to_id`.
    pub fn path_between(&self, from_id: NodeId, to_id: NodeId) -> Result<PathBetween, StoreError> {
        let distance = self.tree_distance(from_id, to_id)?;
        let nodes = query_nodes(
            self.connection(),
            &format!(
                "WITH RECURSIVE
                 left_path(id,parent_id,distance) AS (
                     SELECT id,parent_id,0 FROM nodes WHERE id=?1
                     UNION ALL SELECT n.id,n.parent_id,left_path.distance+1
                     FROM nodes n JOIN left_path ON n.id=left_path.parent_id
                 ),
                 right_path(id,parent_id,distance) AS (
                     SELECT id,parent_id,0 FROM nodes WHERE id=?2
                     UNION ALL SELECT n.id,n.parent_id,right_path.distance+1
                     FROM nodes n JOIN right_path ON n.id=right_path.parent_id
                 ),
                 route(id,phase,position) AS (
                     SELECT id,0,distance FROM left_path
                     WHERE distance<=(SELECT distance FROM left_path WHERE id=?3)
                     UNION ALL
                     SELECT id,1,distance FROM right_path
                     WHERE distance<(SELECT distance FROM right_path WHERE id=?3)
                 )
                 SELECT {NODE_COLUMNS} FROM route JOIN nodes n ON n.id=route.id
                 ORDER BY route.phase,
                          CASE WHEN route.phase=0 THEN route.position ELSE -route.position END"
            ),
            params![
                from_id.to_string(),
                to_id.to_string(),
                distance.lowest_common_ancestor_id.to_string()
            ],
        )?;
        Ok(PathBetween {
            from_id,
            to_id,
            lowest_common_ancestor_id: distance.lowest_common_ancestor_id,
            distance: distance.distance,
            nodes: project_canonical_nodes(&nodes),
        })
    }

    /// Tests reflexive ancestor containment with an upward recursive query.
    pub fn contains_ancestor(
        &self,
        ancestor_id: NodeId,
        descendant_id: NodeId,
    ) -> Result<AncestorContainment, StoreError> {
        for id in [ancestor_id, descendant_id] {
            let exists = self.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1)",
                [id.to_string()],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(StoreError::NotFound(id.to_string()));
            }
        }
        let contains = self.connection().query_row(
            "WITH RECURSIVE path(id,parent_id) AS (
                 SELECT id,parent_id FROM nodes WHERE id=?2
                 UNION ALL
                 SELECT n.id,n.parent_id FROM nodes n JOIN path ON n.id=path.parent_id
             ) SELECT EXISTS(SELECT 1 FROM path WHERE id=?1)",
            params![ancestor_id.to_string(), descendant_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(AncestorContainment {
            ancestor_id,
            descendant_id,
            contains,
        })
    }

    /// Returns the reflexive lowest common ancestor of two canonical nodes.
    pub fn lowest_common_ancestor(
        &self,
        left_id: NodeId,
        right_id: NodeId,
    ) -> Result<NodeProjection, StoreError> {
        for id in [left_id, right_id] {
            let exists = self.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1)",
                [id.to_string()],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(StoreError::NotFound(id.to_string()));
            }
        }
        let lca = self.connection().query_row(
            "WITH RECURSIVE
             left_path(id,parent_id,distance) AS (
                 SELECT id,parent_id,0 FROM nodes WHERE id=?1
                 UNION ALL SELECT n.id,n.parent_id,left_path.distance+1
                 FROM nodes n JOIN left_path ON n.id=left_path.parent_id
             ),
             right_path(id,parent_id,distance) AS (
                 SELECT id,parent_id,0 FROM nodes WHERE id=?2
                 UNION ALL SELECT n.id,n.parent_id,right_path.distance+1
                 FROM nodes n JOIN right_path ON n.id=right_path.parent_id
             )
             SELECT left_path.id FROM left_path JOIN right_path USING(id)
             ORDER BY left_path.distance+right_path.distance,left_path.id LIMIT 1",
            params![left_id.to_string(), right_id.to_string()],
            |row| row.get::<_, String>(0),
        )?;
        let id =
            NodeId::from_str(&lca).map_err(|error| StoreError::InvalidData(error.to_string()))?;
        self.get(id)?
            .as_ref()
            .map(NodeProjection::from)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    /// Computes compact subtree aggregates entirely in `SQLite`.
    pub fn tree_statistics(&self, id: NodeId) -> Result<TreeStatistics, StoreError> {
        let row = self.connection().query_row(
            "WITH RECURSIVE tree(id,depth) AS (
                 SELECT id,0 FROM nodes WHERE id=?1
                 UNION ALL
                 SELECT n.id,tree.depth+1 FROM nodes n JOIN tree ON n.parent_id=tree.id
             ), levels AS (
                 SELECT depth,COUNT(*) AS width,
                        SUM(NOT EXISTS(SELECT 1 FROM nodes child WHERE child.parent_id=tree.id)) AS leaves
                 FROM tree GROUP BY depth
             )
             SELECT
                 (SELECT COUNT(*) FROM nodes WHERE parent_id=?1),
                 COALESCE(SUM(width),0),COALESCE(SUM(leaves),0),
                 COALESCE(MAX(depth),0),COALESCE(MAX(width),0)
             FROM levels",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;
        let (direct, size, leaves, depth, width) = row;
        if size == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(TreeStatistics {
            node_id: id,
            direct_child_count: u64::try_from(direct)
                .map_err(|_| StoreError::InvalidData("direct child count".into()))?,
            subtree_size: u64::try_from(size)
                .map_err(|_| StoreError::InvalidData("subtree size".into()))?,
            leaf_count: u64::try_from(leaves)
                .map_err(|_| StoreError::InvalidData("leaf count".into()))?,
            max_relative_depth: u32::try_from(depth)
                .map_err(|_| StoreError::InvalidData("maximum relative depth".into()))?,
            max_width: u64::try_from(width)
                .map_err(|_| StoreError::InvalidData("maximum width".into()))?,
        })
    }

    /// Returns a stable resumable page of read-only integrity findings.
    pub fn integrity_page(
        &self,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<IntegrityPage, PageReadError> {
        let revision = self.workspace_revision()?;
        let scope = CursorScope::new("validate", None, "code_node_detail")?;
        let start = cursor
            .map(|value| value.resume(&scope, revision))
            .transpose()?
            .map(|position| match position {
                PagePosition::Search { offset } => Ok(offset),
                _ => Err(PaginationError::InvalidCursorPosition),
            })
            .transpose()?
            .unwrap_or(0);
        let mut findings = self.validate_integrity()?.findings;
        findings.sort_by(|left, right| {
            (left.code, left.node_id, left.detail.as_str()).cmp(&(
                right.code,
                right.node_id,
                right.detail.as_str(),
            ))
        });
        let total_findings_usize = findings.len();
        let total_findings = u64::try_from(total_findings_usize)
            .map_err(|_| StoreError::InvalidData("integrity finding count".into()))?;
        let start_index = usize::try_from(start)
            .map_err(|_| StoreError::InvalidData("integrity page offset".into()))?;
        if start_index > findings.len() {
            return Err(PaginationError::InvalidCursorPosition.into());
        }
        let requested = usize::try_from(limit.get())
            .map_err(|_| StoreError::InvalidData("integrity page limit".into()))?;
        let items = findings
            .into_iter()
            .skip(start_index)
            .take(requested)
            .collect::<Vec<_>>();
        let consumed = start_index + items.len();
        let next_cursor = if consumed < total_findings_usize {
            Some(PageCursor::issue(
                revision,
                scope,
                PagePosition::Search {
                    offset: u32::try_from(consumed)
                        .map_err(|_| StoreError::InvalidData("integrity page offset".into()))?,
                },
            )?)
        } else {
            None
        };
        Ok(IntegrityPage {
            healthy: total_findings == 0,
            total_findings,
            page: Page::new(items, next_cursor),
        })
    }

    /// Returns retained revisions newest-first without loading historical bodies.
    pub fn revision_history_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<RevisionSummary>, PageReadError> {
        let transaction = self
            .connection()
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        let revision = transaction_workspace_revision(&transaction)?;
        let exists = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1)",
                [id.to_string()],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)?;
        if !exists {
            return Err(StoreError::NotFound(id.to_string()).into());
        }
        let scope = CursorScope::new("revision_history", Some(id), "newest_first")?;
        let start = cursor
            .map(|value| value.resume(&scope, revision))
            .transpose()?
            .map(|position| match position {
                PagePosition::Search { offset } => Ok(offset),
                _ => Err(PaginationError::InvalidCursorPosition),
            })
            .transpose()?
            .unwrap_or(0);
        let row_limit = i64::from(limit.get()) + 1;
        let sql =
            "SELECT node_id,version,created_by,created_at,change_summary,content_hash,revision_hash
                   FROM node_versions WHERE node_id=?1
                   ORDER BY version DESC LIMIT ?2 OFFSET ?3";
        let mut statement = transaction.prepare(sql).map_err(StoreError::from)?;
        let rows = statement
            .query_map(
                params![id.to_string(), row_limit, i64::from(start)],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, Vec<u8>>(6)?,
                    ))
                },
            )
            .map_err(StoreError::from)?;
        let mut items = rows
            .map(|row| {
                let (node_id, version, created_by, created_at, change_summary, content, revision) =
                    row?;
                Ok(RevisionSummary {
                    node_id: NodeId::from_str(&node_id)
                        .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                    version: u64::try_from(version)
                        .map_err(|_| StoreError::InvalidData("revision version".into()))?,
                    created_by,
                    created_at: u64::try_from(created_at)
                        .map_err(|_| StoreError::InvalidData("revision timestamp".into()))?,
                    change_summary,
                    content_hash: bytes_hash(&content)?,
                    revision_hash: bytes_hash(&revision)?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let requested = usize::try_from(limit.get())
            .map_err(|_| StoreError::InvalidData("revision page limit".into()))?;
        let has_more = items.len() > requested;
        if has_more {
            items.pop();
        }
        let next_cursor = if has_more {
            Some(PageCursor::issue(
                revision,
                scope,
                PagePosition::Search {
                    offset: start
                        .checked_add(
                            u32::try_from(items.len()).map_err(|_| {
                                StoreError::InvalidData("revision page size".into())
                            })?,
                        )
                        .ok_or_else(|| StoreError::InvalidData("revision history offset".into()))?,
                },
            )?)
        } else {
            None
        };
        drop(statement);
        transaction.commit().map_err(StoreError::from)?;
        Ok(Page::new(items, next_cursor))
    }

    /// Returns one bounded deterministic page comparing two current subtrees.
    pub fn subtree_diff_page(
        &self,
        from_root: NodeId,
        to_root: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<SubtreeDiffItem>, PageReadError> {
        let transaction = self
            .connection()
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        let revision = transaction_workspace_revision(&transaction)?;
        let scope = CursorScope::new(
            "subtree_diff",
            Some(from_root),
            &format!("to_root={to_root};stable_id_then_relative_path"),
        )?;
        let start = cursor
            .map(|cursor| cursor.resume(&scope, revision))
            .transpose()?
            .map_or(Ok(0_usize), |position| match position {
                PagePosition::Search { offset } => {
                    usize::try_from(offset).map_err(|_| PaginationError::InvalidCursorPosition)
                }
                PagePosition::Sibling { .. }
                | PagePosition::Traversal { .. }
                | PagePosition::BreadthTraversal { .. }
                | PagePosition::Reference { .. } => Err(PaginationError::InvalidCursorPosition),
            })?;
        let from_nodes = query_subtree_nodes(&transaction, from_root)?;
        let to_nodes = query_subtree_nodes(&transaction, to_root)?;
        if from_nodes.is_empty() {
            return Err(StoreError::NotFound(from_root.to_string()).into());
        }
        if to_nodes.is_empty() {
            return Err(StoreError::NotFound(to_root.to_string()).into());
        }
        let differences = diff_subtrees(from_root, &from_nodes, to_root, &to_nodes);
        if start > differences.len() {
            return Err(PaginationError::InvalidCursorPosition.into());
        }
        let requested = usize::try_from(limit.get())
            .map_err(|_| StoreError::InvalidData("subtree diff page limit".into()))?;
        let mut items = differences
            .into_iter()
            .skip(start)
            .take(requested + 1)
            .collect::<Vec<_>>();
        let has_more = items.len() > requested;
        if has_more {
            items.pop();
        }
        let next_cursor = if has_more {
            let offset = u32::try_from(start + items.len())
                .map_err(|_| StoreError::InvalidData("subtree diff offset".into()))?;
            Some(PageCursor::issue(
                revision,
                scope,
                PagePosition::Search { offset },
            )?)
        } else {
            None
        };
        transaction.commit().map_err(StoreError::from)?;
        Ok(Page::new(items, next_cursor))
    }

    /// Returns the canonical root through the targeted projection contract.
    pub fn root_projection(&self) -> Result<NodeProjection, StoreError> {
        self.root().map(|node| NodeProjection::from(&node))
    }

    /// Resolves and projects one canonical head without loading unrelated state.
    pub fn resolve_projection(
        &self,
        selector: &NodeSelector,
    ) -> Result<Option<NodeProjection>, StoreError> {
        self.resolve(selector)
            .map(|node| node.as_ref().map(NodeProjection::from))
    }

    /// Returns the selected node's direct parent through the targeted projection contract.
    pub fn parent_projection(&self, id: NodeId) -> Result<Option<NodeProjection>, StoreError> {
        self.parent(id)
            .map(|node| node.as_ref().map(NodeProjection::from))
    }

    /// Returns direct children in canonical `(sibling_order, id)` order.
    pub fn children_projections(&self, id: NodeId) -> Result<Vec<NodeProjection>, StoreError> {
        self.children(id)
            .map(|nodes| project_canonical_nodes(&nodes))
    }

    /// Returns siblings, including the selected node, in canonical order.
    pub fn sibling_projections(&self, id: NodeId) -> Result<Vec<NodeProjection>, StoreError> {
        self.siblings(id)
            .map(|nodes| project_canonical_nodes(&nodes))
    }

    /// Returns one resumable page of direct children in canonical sibling order.
    pub fn children_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeProjection>, PageReadError> {
        self.ordered_node_page("children", id, Some(id), false, limit, cursor)
    }

    /// Returns one resumable page of siblings, including the selected node.
    pub fn siblings_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeProjection>, PageReadError> {
        self.ordered_node_page("siblings", id, None, true, limit, cursor)
    }

    fn ordered_node_page(
        &self,
        operation: &str,
        selected_id: NodeId,
        supplied_parent_id: Option<NodeId>,
        resolve_selected_parent: bool,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeProjection>, PageReadError> {
        let transaction = self
            .connection()
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        let revision = transaction_workspace_revision(&transaction)?;
        let parent_id = if resolve_selected_parent {
            query_nodes(
                &transaction,
                &format!("{SELECT_NODE} WHERE n.id=?1"),
                [selected_id.to_string()],
            )?
            .pop()
            .ok_or_else(|| StoreError::NotFound(selected_id.to_string()))?
            .parent_id()
        } else {
            supplied_parent_id
        };
        let scope = CursorScope::new(operation, Some(selected_id), "canonical_sibling_order")?;
        let after = cursor
            .map(|cursor| cursor.resume(&scope, revision))
            .transpose()?
            .map(|position| match position {
                PagePosition::Sibling {
                    sibling_order,
                    node_id,
                } => Ok((sibling_order, node_id)),
                PagePosition::Traversal { .. }
                | PagePosition::BreadthTraversal { .. }
                | PagePosition::Search { .. }
                | PagePosition::Reference { .. } => Err(PaginationError::InvalidCursorPosition),
            })
            .transpose()?;
        let row_limit = i64::from(limit.get()) + 1;
        let mut nodes = match (parent_id, after) {
            (Some(parent), Some((order, node_id))) => query_nodes(
                &transaction,
                &format!(
                    "{SELECT_NODE} WHERE n.parent_id=?1 AND
                     (n.sibling_order>?2 OR (n.sibling_order=?2 AND n.id>?3))
                     ORDER BY n.sibling_order,n.id LIMIT ?4"
                ),
                params![parent.to_string(), order, node_id.to_string(), row_limit],
            )?,
            (Some(parent), None) => query_nodes(
                &transaction,
                &format!(
                    "{SELECT_NODE} WHERE n.parent_id=?1
                     ORDER BY n.sibling_order,n.id LIMIT ?2"
                ),
                params![parent.to_string(), row_limit],
            )?,
            (None, Some((order, node_id))) => query_nodes(
                &transaction,
                &format!(
                    "{SELECT_NODE} WHERE n.id=?1 AND
                     (n.sibling_order>?2 OR (n.sibling_order=?2 AND n.id>?3))
                     ORDER BY n.sibling_order,n.id LIMIT ?4"
                ),
                params![
                    selected_id.to_string(),
                    order,
                    node_id.to_string(),
                    row_limit
                ],
            )?,
            (None, None) => query_nodes(
                &transaction,
                &format!("{SELECT_NODE} WHERE n.id=?1 LIMIT ?2"),
                params![selected_id.to_string(), row_limit],
            )?,
        };
        let has_more = nodes.len() > usize::try_from(limit.get()).expect("page limit fits usize");
        if has_more {
            nodes.pop();
        }
        let next_cursor = if has_more {
            nodes
                .last()
                .map(|node| {
                    PageCursor::issue(
                        revision,
                        scope,
                        PagePosition::Sibling {
                            sibling_order: node.fields().sibling_order,
                            node_id: node.id(),
                        },
                    )
                })
                .transpose()?
        } else {
            None
        };
        transaction.commit().map_err(StoreError::from)?;
        Ok(Page::new(project_canonical_nodes(&nodes), next_cursor))
    }

    /// Returns one resumable page of descendants in canonical DFS order.
    pub fn descendants_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        self.descendants_page_ordered(id, TraversalOrder::Dfs, limit, cursor)
    }

    /// Returns one resumable page of descendants in the selected database-side order.
    pub fn descendants_page_ordered(
        &self,
        id: NodeId,
        order: TraversalOrder,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        self.traversal_page("descendants", id, false, None, order, limit, cursor)
    }

    /// Returns one resumable page containing a node and its canonical DFS descendants.
    pub fn subtree_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        self.subtree_page_ordered(id, TraversalOrder::Dfs, limit, cursor)
    }

    /// Returns one resumable page containing a node and descendants in the selected order.
    pub fn subtree_page_ordered(
        &self,
        id: NodeId,
        order: TraversalOrder,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        self.traversal_page("subtree", id, true, None, order, limit, cursor)
    }

    /// Returns a resumable DFS page filtered by canonical child existence.
    pub fn filtered_subtree_page(
        &self,
        id: NodeId,
        predicate: StructuralPredicate,
        max_depth: Option<u32>,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        let transaction = self
            .connection()
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        let revision = transaction_workspace_revision(&transaction)?;
        let request_key = max_depth.map_or_else(
            || format!("canonical_dfs_order;predicate={}", predicate.as_str()),
            |depth| {
                format!(
                    "canonical_dfs_order;predicate={};max_depth={depth}",
                    predicate.as_str()
                )
            },
        );
        let scope = CursorScope::new("filter_nodes", Some(id), &request_key)?;
        let after = cursor
            .map(|value| value.resume(&scope, revision))
            .transpose()?
            .map(|position| match position {
                PagePosition::Traversal {
                    ordering_path,
                    node_id: _,
                } => Ok(ordering_path),
                _ => Err(PaginationError::InvalidCursorPosition),
            })
            .transpose()?;
        let after_path = after.as_deref();
        let output_limit = i64::from(limit.get()) + 1;
        let ordering_expression = "x.ordering||'/'||printf('%010d-%s',n.sibling_order,n.id)";
        let sql = format!(
            "WITH RECURSIVE x(id,depth,ordering) AS (
             SELECT id,0,printf('%010d-%s',sibling_order,id) FROM nodes WHERE id=?1
             UNION ALL
             SELECT n.id,x.depth+1,{ordering_expression}
             FROM nodes n JOIN x ON n.parent_id=x.id
             WHERE (?2 IS NULL OR x.depth<?2)
             ) SELECT {NODE_COLUMNS},x.depth,x.ordering
             FROM x JOIN nodes n ON n.id=x.id
             WHERE (?3 IS NULL OR x.ordering>?3) AND
                   ((?4='leaf' AND NOT EXISTS(SELECT 1 FROM nodes c WHERE c.parent_id=n.id)) OR
                    (?4='internal' AND EXISTS(SELECT 1 FROM nodes c WHERE c.parent_id=n.id)))
             ORDER BY x.ordering LIMIT ?5"
        );
        let mut rows = query_ordered_depths(
            &transaction,
            &sql,
            params![
                id.to_string(),
                max_depth.map(i64::from),
                after_path,
                predicate.as_str(),
                output_limit
            ],
        )?;
        if rows.is_empty() && self.get(id)?.is_none() {
            return Err(StoreError::NotFound(id.to_string()).into());
        }
        let requested = usize::try_from(limit.get())
            .map_err(|_| StoreError::InvalidData("filter page limit".into()))?;
        let has_more = rows.len() > requested;
        if has_more {
            rows.pop();
        }
        let next_cursor = if has_more {
            rows.last()
                .map(|(row, ordering_path)| {
                    PageCursor::issue(
                        revision,
                        scope,
                        PagePosition::Traversal {
                            ordering_path: ordering_path.clone(),
                            node_id: row.node.id(),
                        },
                    )
                })
                .transpose()?
        } else {
            None
        };
        transaction.commit().map_err(StoreError::from)?;
        Ok(Page::new(
            self.project_depths(rows.into_iter().map(|(row, _)| row).collect()),
            next_cursor,
        ))
    }

    /// Returns one resumable bounded-depth DFS page for compact inspection.
    pub(crate) fn inspection_traversal_page(
        &self,
        operation: &str,
        id: NodeId,
        max_depth: u32,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        self.traversal_page(
            operation,
            id,
            true,
            Some(max_depth),
            TraversalOrder::Dfs,
            limit,
            cursor,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn traversal_page(
        &self,
        operation: &str,
        id: NodeId,
        include_root: bool,
        max_depth: Option<u32>,
        order: TraversalOrder,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<NodeDepthProjection>, PageReadError> {
        let transaction = self
            .connection()
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        let revision = transaction_workspace_revision(&transaction)?;
        let request_key = max_depth.map_or_else(
            || format!("canonical_{}_order", order.as_str()),
            |depth| format!("canonical_{}_order;max_depth={depth}", order.as_str()),
        );
        let scope = CursorScope::new(operation, Some(id), &request_key)?;
        let after = cursor
            .map(|cursor| cursor.resume(&scope, revision))
            .transpose()?
            .map(|position| match (order, position) {
                (
                    TraversalOrder::Dfs,
                    PagePosition::Traversal {
                        ordering_path,
                        node_id,
                    },
                ) => Ok((None, ordering_path, node_id)),
                (
                    TraversalOrder::Bfs,
                    PagePosition::BreadthTraversal {
                        depth,
                        ordering_path,
                        node_id,
                    },
                ) => Ok((Some(depth), ordering_path, node_id)),
                _ => Err(PaginationError::InvalidCursorPosition),
            })
            .transpose()?;
        let after_depth = after.as_ref().and_then(|(depth, _, _)| *depth);
        let after_path = after.as_ref().map(|(_, path, _)| path.as_str());
        let scaffold_count =
            after_path.map_or(usize::from(!include_root), |path| path.split('/').count());
        let lookahead = usize::try_from(limit.get()).expect("page limit fits usize") + 1;
        let recursive_limit = i64::try_from(scaffold_count + lookahead)
            .map_err(|_| StoreError::InvalidData("traversal page limit".into()))?;
        let output_limit = i64::from(limit.get()) + 1;
        let minimum_depth = u32::from(!include_root);
        let ordering_expression = "x.ordering||'/'||printf('%010d-%s',n.sibling_order,n.id)";
        let dfs_sql = format!(
            "WITH RECURSIVE x(id,depth,ordering) AS (
             SELECT id,0,printf('%010d-%s',sibling_order,id) FROM nodes WHERE id=?1
             UNION ALL
             SELECT n.id,x.depth+1,{ordering_expression}
             FROM nodes n JOIN x ON n.parent_id=x.id
             WHERE (?6 IS NULL OR x.depth<?6) AND
                (?2 IS NULL OR {ordering_expression}=?2
                 OR ?2 LIKE {ordering_expression}||'/%' OR {ordering_expression}>?2)
             ORDER BY 3 LIMIT ?3
             ) SELECT {NODE_COLUMNS},x.depth,x.ordering
             FROM x JOIN nodes n ON n.id=x.id
             WHERE (?2 IS NULL AND x.depth>=?4) OR (?2 IS NOT NULL AND x.ordering>?2)
             ORDER BY x.ordering LIMIT ?5"
        );
        let bfs_sql = format!(
            "WITH RECURSIVE x(id,depth,ordering) AS (
             SELECT id,0,printf('%010d-%s',sibling_order,id) FROM nodes WHERE id=?1
             UNION ALL
             SELECT n.id,x.depth+1,{ordering_expression}
             FROM nodes n JOIN x ON n.parent_id=x.id
             WHERE (?6 IS NULL OR x.depth<?6)
             ) SELECT {NODE_COLUMNS},x.depth,x.ordering
             FROM x JOIN nodes n ON n.id=x.id
             WHERE x.depth>=?4 AND (?2 IS NULL OR x.depth>?2 OR (x.depth=?2 AND x.ordering>?3))
             ORDER BY x.depth,x.ordering LIMIT ?5"
        );
        let mut rows = match order {
            TraversalOrder::Dfs => query_ordered_depths(
                &transaction,
                &dfs_sql,
                params![
                    id.to_string(),
                    after_path,
                    recursive_limit,
                    minimum_depth,
                    output_limit,
                    max_depth.map(i64::from)
                ],
            )?,
            TraversalOrder::Bfs => query_ordered_depths(
                &transaction,
                &bfs_sql,
                params![
                    id.to_string(),
                    after_depth,
                    after_path,
                    minimum_depth,
                    output_limit,
                    max_depth.map(i64::from)
                ],
            )?,
        };
        let has_more = rows.len() > usize::try_from(limit.get()).expect("page limit fits usize");
        if has_more {
            rows.pop();
        }
        let next_cursor = if has_more {
            rows.last()
                .map(|(row, ordering_path)| {
                    let position = match order {
                        TraversalOrder::Dfs => PagePosition::Traversal {
                            ordering_path: ordering_path.clone(),
                            node_id: row.node.id(),
                        },
                        TraversalOrder::Bfs => PagePosition::BreadthTraversal {
                            depth: row.depth,
                            ordering_path: ordering_path.clone(),
                            node_id: row.node.id(),
                        },
                    };
                    PageCursor::issue(revision, scope, position)
                })
                .transpose()?
        } else {
            None
        };
        transaction.commit().map_err(StoreError::from)?;
        Ok(Page::new(
            self.project_depths(rows.into_iter().map(|(row, _)| row).collect()),
            next_cursor,
        ))
    }

    /// Loads only the requested canonical heads in caller-provided order.
    ///
    /// Missing identities are omitted, matching the established adapter
    /// projection behavior. Duplicate requested identities remain duplicated.
    pub fn project_nodes(&self, ids: &[NodeId]) -> Result<Vec<NodeProjection>, StoreError> {
        let nodes = ids
            .iter()
            .map(|id| self.get(*id))
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(project_canonical_nodes(&nodes))
    }

    /// Projects already-loaded traversal rows in their supplied order.
    pub fn project_depths(&self, rows: Vec<NodeDepth>) -> Vec<NodeDepthProjection> {
        rows.into_iter()
            .map(|row| NodeDepthProjection {
                depth: row.depth,
                node: NodeProjection::from(&row.node),
            })
            .collect()
    }

    /// Projects one compact summary using the canonical breadcrumb service.
    pub fn project_summary(&self, node: &Node) -> Result<ContextSummary, StoreError> {
        self.summary(node)
    }

    /// Returns direct-child counts in caller-provided order with one aggregate query.
    pub fn child_counts(&self, ids: &[NodeId]) -> Result<Vec<NodeChildCount>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let values = (1..=ids.len())
            .map(|position| format!("(?{position},{})", position - 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH requested(id,position) AS (VALUES {values})
             SELECT requested.id,COUNT(children.id)
             FROM requested LEFT JOIN nodes children ON children.parent_id=requested.id
             GROUP BY requested.position,requested.id ORDER BY requested.position"
        );
        let parameters = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut statement = self.connection().prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.map(|row| {
            let (id, count) = row?;
            Ok(NodeChildCount {
                node_id: NodeId::from_str(&id)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                child_count: u32::try_from(count)
                    .map_err(|_| StoreError::InvalidData("child count".into()))?,
            })
        })
        .collect()
    }
}

fn bytes_hash(bytes: &[u8]) -> Result<mdtree_core::NodeHash, StoreError> {
    let value: [u8; 32] = bytes
        .try_into()
        .map_err(|_| StoreError::InvalidData("32-byte node hash".into()))?;
    Ok(mdtree_core::NodeHash::new(value))
}

fn relationship_distances(
    store: &SqliteStore,
    from_id: NodeId,
    to_id: NodeId,
) -> Result<(NodeId, u32, u32), StoreError> {
    let result = store
        .connection()
        .query_row(
            "WITH RECURSIVE
             left_path(id,parent_id,distance) AS (
                 SELECT id,parent_id,0 FROM nodes WHERE id=?1
                 UNION ALL SELECT n.id,n.parent_id,left_path.distance+1
                 FROM nodes n JOIN left_path ON n.id=left_path.parent_id
             ),
             right_path(id,parent_id,distance) AS (
                 SELECT id,parent_id,0 FROM nodes WHERE id=?2
                 UNION ALL SELECT n.id,n.parent_id,right_path.distance+1
                 FROM nodes n JOIN right_path ON n.id=right_path.parent_id
             )
             SELECT left_path.id,left_path.distance,right_path.distance
             FROM left_path JOIN right_path USING(id)
             ORDER BY left_path.distance+right_path.distance,left_path.id LIMIT 1",
            params![from_id.to_string(), to_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => {
                StoreError::NotFound(format!("{from_id} or {to_id}"))
            }
            other => StoreError::from(other),
        })?;
    Ok((
        NodeId::from_str(&result.0).map_err(|error| StoreError::InvalidData(error.to_string()))?,
        u32::try_from(result.1).map_err(|_| StoreError::InvalidData("left distance".into()))?,
        u32::try_from(result.2).map_err(|_| StoreError::InvalidData("right distance".into()))?,
    ))
}

fn transaction_workspace_revision(connection: &rusqlite::Connection) -> Result<u64, StoreError> {
    let revision = connection.query_row(
        "SELECT revision FROM workspace WHERE singleton=1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(revision).map_err(|_| StoreError::InvalidData("workspace revision".into()))
}

fn query_subtree_nodes(
    connection: &rusqlite::Connection,
    root: NodeId,
) -> Result<Vec<NodeProjection>, StoreError> {
    let rows = query_ordered_depths(
        connection,
        &format!(
            "WITH RECURSIVE x(id,depth,ordering) AS (
             SELECT id,0,printf('%010d-%s',sibling_order,id) FROM nodes WHERE id=?1
             UNION ALL
             SELECT n.id,x.depth+1,x.ordering||'/'||printf('%010d-%s',n.sibling_order,n.id)
             FROM nodes n JOIN x ON n.parent_id=x.id
             ) SELECT {NODE_COLUMNS},x.depth,x.ordering
             FROM x JOIN nodes n ON n.id=x.id ORDER BY x.ordering"
        ),
        [root.to_string()],
    )?;
    Ok(rows
        .into_iter()
        .map(|(row, _)| NodeProjection::from(&row.node))
        .collect())
}

#[cfg(test)]
mod tests {
    use mdtree_core::{
        generate_large_tree_fixture, LargeTreeFixtureSpec, NodeProjection, NodeSelector, PageLimit,
        PaginationErrorCode, SubtreeChange,
    };
    use tempfile::tempdir;

    use crate::{import_snapshot_new, test_support::open_observed_store};

    fn fixture() -> mdtree_core::LargeTreeFixture {
        generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 24,
                deep_descendants: 8,
                history_revisions: 9,
                relations: 20,
                response_boundary_bytes: 4096,
            },
            92,
        )
    }

    #[test]
    fn subtree_diff_pages_are_bounded_resumable_and_structural() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("subtree-diff-pages.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 128,
                deep_descendants: 8,
                history_revisions: 4,
                relations: 8,
                response_boundary_bytes: 4096,
            },
            119,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let store = crate::SqliteStore::open(&path).expect("store");
        let limit = PageLimit::new(17).expect("limit");
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page = store
                .subtree_diff_page(
                    fixture.root_id,
                    fixture.wide_parent_id,
                    limit,
                    cursor.as_ref(),
                )
                .expect("diff page");
            assert!(page.items.len() <= 17);
            items.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                assert!(page.complete);
                break;
            }
            assert!(page.truncated);
        }
        assert!(
            items.len() > 100,
            "fixture must cross the maximum page size"
        );
        assert!(items
            .iter()
            .any(|item| item.changes.contains(&SubtreeChange::Moved)));
        assert!(items
            .iter()
            .any(|item| item.changes.contains(&SubtreeChange::Removed)));
    }

    #[test]
    fn selected_nodes_and_depth_rows_preserve_exact_fields_and_caller_order() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("projection.mdtree");
        let fixture = fixture();
        import_snapshot_new(&path, &fixture.snapshot).expect("scale import");
        let store = crate::SqliteStore::open(&path).expect("store");
        let requested = [
            fixture.wide_child_ids[7],
            fixture.history_node_id,
            fixture.wide_child_ids[1],
            fixture.wide_child_ids[7],
        ];

        let projected = store.project_nodes(&requested).expect("targeted nodes");
        assert_eq!(
            projected.iter().map(|node| node.id).collect::<Vec<_>>(),
            requested
        );
        for node in &projected {
            let expected = fixture
                .snapshot
                .nodes
                .iter()
                .find(|candidate| candidate.id == node.id)
                .expect("fixture node");
            assert_eq!(node, expected);
        }

        let depth_rows = store
            .descendants(fixture.deep_parent_id)
            .expect("deep rows");
        let expected = depth_rows
            .iter()
            .map(|row| (row.depth, row.node.id()))
            .collect::<Vec<_>>();
        let projected = store.project_depths(depth_rows);
        assert_eq!(
            projected
                .iter()
                .map(|row| (row.depth, row.node.id))
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn batch_node_lookup_preserves_order_duplicates_and_per_item_errors() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("batch-node.mdtree");
        let fixture = fixture();
        import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let store = crate::SqliteStore::open(&path).expect("store");
        let root = fixture.root_id.to_string();
        let selectors = vec![
            root.clone(),
            "wide-00000".into(),
            root,
            "missing-node".into(),
            " ".into(),
        ];
        let items = store.batch_node_lookup(&selectors).expect("batch lookup");
        assert_eq!(items.len(), selectors.len());
        assert_eq!(items[0].node.as_ref().expect("root").id, fixture.root_id);
        assert_eq!(
            items[1].node.as_ref().expect("slug result").id,
            fixture.wide_child_ids[0]
        );
        assert_eq!(
            items[2].node.as_ref().expect("duplicate root").id,
            fixture.root_id
        );
        assert_eq!(items[3].error.as_ref().expect("missing").code, "not_found");
        assert_eq!(
            items[4].error.as_ref().expect("invalid").code,
            "invalid_selector"
        );
        assert!(store.batch_node_lookup(&vec!["root".into(); 101]).is_err());

        let requests = vec![
            mdtree_core::BatchChildrenRequest {
                parent: fixture.wide_parent_id.to_string(),
                limit: 10,
                cursor: None,
            },
            mdtree_core::BatchChildrenRequest {
                parent: fixture.root_id.to_string(),
                limit: 5,
                cursor: None,
            },
            mdtree_core::BatchChildrenRequest {
                parent: "missing-node".into(),
                limit: 1,
                cursor: None,
            },
        ];
        let groups = store
            .batch_children_lookup(&requests)
            .expect("batch children");
        assert_eq!(groups.len(), 3);
        let wide_page = groups[0].page.as_ref().expect("wide page");
        assert_eq!(wide_page.items.len(), 10);
        assert!(wide_page.next_cursor.is_some());
        assert_eq!(groups[1].page.as_ref().expect("root page").items.len(), 5);
        assert_eq!(groups[2].error.as_ref().expect("missing").code, "not_found");
        let continued = store
            .batch_children_lookup(&[mdtree_core::BatchChildrenRequest {
                parent: fixture.wide_parent_id.to_string(),
                limit: 10,
                cursor: wide_page.next_cursor.as_ref().map(ToString::to_string),
            }])
            .expect("continued batch children");
        assert_eq!(
            continued[0].page.as_ref().expect("continued").items.len(),
            10
        );
        assert!(store
            .batch_children_lookup(&[
                mdtree_core::BatchChildrenRequest {
                    parent: fixture.root_id.to_string(),
                    limit: 51,
                    cursor: None,
                },
                mdtree_core::BatchChildrenRequest {
                    parent: fixture.wide_parent_id.to_string(),
                    limit: 50,
                    cursor: None,
                },
            ])
            .is_err());
    }

    #[test]
    fn atomic_subtree_clone_remaps_internal_explicit_references() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("clone-subtree.mdtree");
        let fixture = fixture();
        import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let mut store = crate::SqliteStore::open(&path).expect("store");
        let request = mdtree_core::CloneSubtreeRequest {
            source_id: fixture.wide_parent_id,
            destination_parent_id: fixture.root_id,
            expected_version: 1,
            sibling_order: Some(0),
            dry_run: true,
            created_at: 500,
            created_by: Some("test".into()),
            change_summary: Some("Clone wide branch".into()),
        };
        let before = store
            .tree_statistics(fixture.root_id)
            .expect("before")
            .subtree_size;
        let planned = store
            .clone_subtree(&request, &mdtree_core::SequentialUlidGenerator::new(50_000))
            .expect("clone plan");
        assert_eq!(planned.status, "planned");
        assert_eq!(planned.node_count, 25);
        assert_eq!(planned.explicit_reference_count, 20);
        assert_eq!(
            store
                .tree_statistics(fixture.root_id)
                .expect("after plan")
                .subtree_size,
            before
        );

        let applied = store
            .clone_subtree(
                &mdtree_core::CloneSubtreeRequest {
                    dry_run: false,
                    ..request.clone()
                },
                &mdtree_core::SequentialUlidGenerator::new(60_000),
            )
            .expect("apply clone");
        let clone_root = applied.cloned_root_id.expect("clone root");
        let cloned = store.subtree(clone_root).expect("cloned subtree");
        assert_eq!(cloned.len(), 25);
        let cloned_ids = cloned
            .iter()
            .map(|row| row.node.id())
            .collect::<std::collections::BTreeSet<_>>();
        let explicit = cloned
            .iter()
            .flat_map(|row| {
                store
                    .outgoing_references(row.node.id())
                    .expect("references")
            })
            .filter(|reference| {
                !matches!(
                    reference.origin,
                    mdtree_core::ReferenceOrigin::Markdown
                        | mdtree_core::ReferenceOrigin::Wikilink
                        | mdtree_core::ReferenceOrigin::Inferred
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(explicit.len(), 20);
        assert!(explicit.iter().all(|reference| match reference.target {
            mdtree_core::ReferenceTarget::Resolved { node_id, .. } => cloned_ids.contains(&node_id),
            mdtree_core::ReferenceTarget::Unresolved { .. } => true,
        }));
        assert_eq!(
            store
                .tree_statistics(fixture.root_id)
                .expect("after")
                .subtree_size,
            before + 25
        );
        assert!(store
            .clone_subtree(
                &mdtree_core::CloneSubtreeRequest {
                    expected_version: 99,
                    ..request
                },
                &mdtree_core::SequentialUlidGenerator::new(70_000),
            )
            .is_err());
    }

    #[test]
    fn projections_and_aggregate_counts_do_not_load_history_or_references() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("observed-projection.mdtree");
        let fixture = fixture();
        import_snapshot_new(&path, &fixture.snapshot).expect("scale import");
        let (store, observer) = open_observed_store(&path).expect("observed store");
        let requested = [
            fixture.history_node_id,
            fixture.wide_parent_id,
            fixture.history_node_id,
        ];

        let projected = store.project_nodes(&requested).expect("targeted nodes");
        assert_eq!(projected.len(), 3);
        let summary_node = store
            .get(fixture.history_node_id)
            .expect("summary read")
            .expect("history node");
        let summary = store
            .project_summary(&summary_node)
            .expect("targeted summary");
        assert_eq!(summary.node_id, fixture.history_node_id);
        assert_eq!(summary.title, summary_node.fields().metadata.title);
        assert_eq!(summary.summary, summary_node.fields().metadata.summary);
        assert_eq!(summary.node_type, summary_node.fields().metadata.node_type);
        let counts = store.child_counts(&requested).expect("child counts");
        assert_eq!(counts[0].node_id, fixture.history_node_id);
        assert_eq!(counts[0].child_count, 0);
        assert_eq!(counts[1].node_id, fixture.wide_parent_id);
        assert_eq!(counts[1].child_count, 24);
        assert_eq!(counts[2].node_id, fixture.history_node_id);
        assert_eq!(counts[2].child_count, 0);

        let observation = observer.observation();
        assert_eq!(observation.table_read_count("node_versions"), 0);
        assert_eq!(observation.table_read_count("references"), 0);
        assert!(!observation.has_complete_snapshot_signature());
        let _: Vec<NodeProjection> = projected;
    }

    #[test]
    fn root_resolved_node_and_parent_reads_are_targeted_and_bounded() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("single-node-projection.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 240,
                deep_descendants: 32,
                history_revisions: 20,
                relations: 80,
                response_boundary_bytes: 4096,
            },
            95,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let (store, observer) = open_observed_store(&path).expect("observed store");

        let root = store.root_projection().expect("root projection");
        assert_eq!(root.id, fixture.root_id);
        assert_targeted_read(&observer.observation(), 1);

        let selectors = [
            (NodeSelector::Id(fixture.history_node_id), 1, "ID selector"),
            (
                "history-heavy".parse().expect("slug selector"),
                1,
                "slug selector",
            ),
            (
                "/history-heavy".parse().expect("path selector"),
                2,
                "path selector",
            ),
        ];
        for (selector, max_selects, label) in selectors {
            observer.reset();
            let node = store
                .resolve_projection(&selector)
                .expect(label)
                .expect("selected node");
            assert_eq!(node.id, fixture.history_node_id, "{label}");
            assert_targeted_read(&observer.observation(), max_selects);
        }

        observer.reset();
        let selected = store
            .resolve(&NodeSelector::Id(fixture.history_node_id))
            .expect("selected child")
            .expect("history node");
        let parent = store
            .parent_projection(selected.id())
            .expect("parent projection")
            .expect("root parent");
        assert_eq!(parent.id, fixture.root_id);
        assert_targeted_read(&observer.observation(), 2);

        observer.reset();
        assert_eq!(
            store
                .parent_projection(fixture.root_id)
                .expect("root parent projection"),
            None
        );
        assert_targeted_read(&observer.observation(), 1);
    }

    #[test]
    fn children_and_siblings_preserve_canonical_order_without_unrelated_reads() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("ordered-projection.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 128,
                deep_descendants: 16,
                history_revisions: 20,
                relations: 160,
                response_boundary_bytes: 4096,
            },
            98,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let (store, observer) = open_observed_store(&path).expect("observed store");

        let children = store
            .children_projections(fixture.wide_parent_id)
            .expect("ordered children");
        assert_canonical_wide_order(&children, &fixture.wide_child_ids);
        assert_targeted_read(&observer.observation(), 1);

        observer.reset();
        let siblings = store
            .sibling_projections(fixture.wide_child_ids[73])
            .expect("ordered siblings");
        assert_canonical_wide_order(&siblings, &fixture.wide_child_ids);
        assert_targeted_read(&observer.observation(), 2);
    }

    #[test]
    fn child_and_sibling_pages_enumerate_wide_sets_exactly_once() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("sibling-pages.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 128,
                deep_descendants: 8,
                history_revisions: 4,
                relations: 16,
                response_boundary_bytes: 4096,
            },
            103,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let store = crate::SqliteStore::open(&path).expect("store");
        let limit = PageLimit::new(17).expect("limit");

        for (operation, selected) in [
            ("children", fixture.wide_parent_id),
            ("siblings", fixture.wide_child_ids[73]),
        ] {
            let mut cursor = None;
            let mut ids = Vec::new();
            loop {
                let page = if operation == "children" {
                    store.children_page(selected, limit, cursor.as_ref())
                } else {
                    store.siblings_page(selected, limit, cursor.as_ref())
                }
                .expect("ordered page");
                ids.extend(page.items.iter().map(|node| node.id));
                assert_eq!(page.complete, page.next_cursor.is_none());
                assert_eq!(page.truncated, page.next_cursor.is_some());
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(ids, fixture.wide_child_ids, "{operation}");
        }
    }

    #[test]
    fn sibling_page_cursors_reject_changed_requests_and_workspace_revisions() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("sibling-cursor-errors.mdtree");
        let fixture = fixture();
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let store = crate::SqliteStore::open(&path).expect("store");
        let limit = PageLimit::new(2).expect("limit");
        let first = store
            .children_page(fixture.wide_parent_id, limit, None)
            .expect("first page");
        let cursor = first.next_cursor.expect("continuation");

        let changed_request = store
            .siblings_page(fixture.wide_child_ids[0], limit, Some(&cursor))
            .expect_err("cursor is bound to children request");
        assert_eq!(
            changed_request.pagination_code().expect("pagination error"),
            PaginationErrorCode::InvalidCursor
        );

        store
            .connection()
            .execute(
                "UPDATE nodes SET updated_at=updated_at+1 WHERE id=?1",
                [fixture.wide_child_ids[0].to_string()],
            )
            .expect("canonical mutation");
        let stale = store
            .children_page(fixture.wide_parent_id, limit, Some(&cursor))
            .expect_err("mutation stales cursor");
        assert_eq!(
            stale.pagination_code().expect("pagination error"),
            PaginationErrorCode::StaleCursor
        );
    }

    #[test]
    fn traversal_pages_equal_complete_dfs_and_bound_first_page_work() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("traversal-pages.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 128,
                deep_descendants: 32,
                history_revisions: 4,
                relations: 16,
                response_boundary_bytes: 4096,
            },
            104,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let (store, observer) = open_observed_store(&path).expect("observed store");
        let expected = store
            .subtree(fixture.root_id)
            .expect("complete subtree")
            .into_iter()
            .map(|row| (row.depth, row.node.id()))
            .collect::<Vec<_>>();
        let complete_work = observer.observation().estimated_vm_steps;

        observer.reset();
        let limit = PageLimit::new(3).expect("limit");
        let first = store
            .subtree_page(fixture.root_id, limit, None)
            .expect("first DFS page");
        let first_work = observer.observation().estimated_vm_steps;
        assert!(
            first_work < complete_work,
            "first={first_work}, complete={complete_work}"
        );

        let mut actual = first
            .items
            .iter()
            .map(|row| (row.depth, row.node.id))
            .collect::<Vec<_>>();
        let mut cursor = first.next_cursor;
        while let Some(next) = cursor {
            let page = store
                .subtree_page(fixture.root_id, limit, Some(&next))
                .expect("continued DFS page");
            actual.extend(page.items.iter().map(|row| (row.depth, row.node.id)));
            cursor = page.next_cursor;
        }
        assert_eq!(actual, expected);

        let expected_descendants = expected[1..].to_vec();
        let mut actual_descendants = Vec::new();
        let mut cursor = None;
        loop {
            let page = store
                .descendants_page(fixture.root_id, limit, cursor.as_ref())
                .expect("descendant page");
            actual_descendants.extend(page.items.iter().map(|row| (row.depth, row.node.id)));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(actual_descendants, expected_descendants);

        let mut expected_bfs = expected.clone();
        expected_bfs.sort_by_key(|(depth, _)| *depth);
        let mut actual_bfs = Vec::new();
        let mut cursor = None;
        loop {
            let page = store
                .subtree_page_ordered(
                    fixture.root_id,
                    mdtree_core::TraversalOrder::Bfs,
                    limit,
                    cursor.as_ref(),
                )
                .expect("BFS page");
            actual_bfs.extend(page.items.iter().map(|row| (row.depth, row.node.id)));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(actual_bfs, expected_bfs);
    }

    #[test]
    fn traversal_rows_are_projected_in_one_targeted_canonical_query() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("targeted-traversal.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 240,
                deep_descendants: 64,
                history_revisions: 20,
                relations: 160,
                response_boundary_bytes: 4096,
            },
            101,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let (store, observer) = open_observed_store(&path).expect("observed store");
        let deep_ids = fixture
            .snapshot
            .nodes
            .iter()
            .filter(|node| {
                node.id == fixture.deep_parent_id || node.slug.as_str().starts_with("deep-")
            })
            .map(|node| node.id)
            .collect::<Vec<_>>();

        let ancestors = store.ancestors(fixture.deep_leaf_id).expect("ancestors");
        let mut expected_ancestors = vec![(fixture.root_id, 65_u32)];
        expected_ancestors.extend(
            deep_ids[..deep_ids.len() - 1]
                .iter()
                .enumerate()
                .map(|(index, id)| (*id, u32::try_from(64 - index).expect("ancestor depth"))),
        );
        assert_eq!(
            ancestors
                .iter()
                .map(|row| (row.node.id(), row.depth))
                .collect::<Vec<_>>(),
            expected_ancestors
        );
        assert_targeted_read(&observer.observation(), 4);

        observer.reset();
        let subtree = store.subtree(fixture.deep_parent_id).expect("subtree");
        assert_eq!(
            subtree
                .iter()
                .map(|row| (row.node.id(), row.depth))
                .collect::<Vec<_>>(),
            deep_ids
                .iter()
                .enumerate()
                .map(|(depth, id)| (*id, u32::try_from(depth).expect("subtree depth")))
                .collect::<Vec<_>>()
        );
        assert_targeted_read(&observer.observation(), 4);

        observer.reset();
        let descendants = store
            .descendants(fixture.deep_parent_id)
            .expect("descendants");
        assert_eq!(
            descendants
                .iter()
                .map(|row| (row.node.id(), row.depth))
                .collect::<Vec<_>>(),
            subtree[1..]
                .iter()
                .map(|row| (row.node.id(), row.depth))
                .collect::<Vec<_>>()
        );
        assert_targeted_read(&observer.observation(), 4);
    }

    #[test]
    fn revision_history_pages_are_bounded_complete_and_newest_first() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("history.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 0,
                deep_descendants: 0,
                history_revisions: 250,
                relations: 0,
                response_boundary_bytes: 128,
            },
            417,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let store = crate::SqliteStore::open(&path).expect("store");
        let limit = PageLimit::new(100).expect("limit");
        let mut cursor = None;
        let mut summaries = Vec::new();
        loop {
            let page = store
                .revision_history_page(fixture.history_node_id, limit, cursor.as_ref())
                .expect("history page");
            summaries.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(summaries.len(), 250);
        assert_eq!(summaries[0].version, 250);
        assert_eq!(summaries[249].version, 1);
        assert!(summaries
            .windows(2)
            .all(|pair| pair[0].version > pair[1].version));
        assert!(summaries
            .iter()
            .all(|item| item.node_id == fixture.history_node_id));
    }

    #[test]
    fn integrity_pages_bound_large_read_only_finding_sets() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("integrity.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 120,
                deep_descendants: 0,
                history_revisions: 1,
                relations: 0,
                response_boundary_bytes: 128,
            },
            418,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let store = crate::SqliteStore::open(&path).expect("store");
        store
            .connection()
            .execute(
                "UPDATE nodes SET content_hash=zeroblob(32) WHERE parent_id=?1",
                [fixture.wide_parent_id.to_string()],
            )
            .expect("inject corruption");
        let before = store
            .connection()
            .query_row(
                "SELECT hex(content_hash) FROM nodes WHERE id=?1",
                [fixture.wide_child_ids[0].to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("corrupt hash");
        let limit = PageLimit::new(37).expect("limit");
        let mut cursor = None;
        let mut findings = Vec::new();
        loop {
            let result = store
                .integrity_page(limit, cursor.as_ref())
                .expect("integrity page");
            assert!(!result.healthy);
            assert_eq!(result.total_findings, 120);
            findings.extend(result.page.items);
            cursor = result.page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(findings.len(), 120);
        assert!(findings
            .iter()
            .all(|finding| finding.code == "content_hash"));
        let after = store
            .connection()
            .query_row(
                "SELECT hex(content_hash) FROM nodes WHERE id=?1",
                [fixture.wide_child_ids[0].to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("corrupt hash after validation");
        assert_eq!(before, after);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn tree_statistics_cover_root_wide_deep_and_leaf_shapes() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("statistics.mdtree");
        let fixture = generate_large_tree_fixture(
            LargeTreeFixtureSpec {
                wide_children: 120,
                deep_descendants: 24,
                history_revisions: 1,
                relations: 0,
                response_boundary_bytes: 128,
            },
            419,
        );
        import_snapshot_new(&path, &fixture.snapshot).expect("fixture import");
        let store = crate::SqliteStore::open(&path).expect("store");
        let root = store.tree_statistics(fixture.root_id).expect("root stats");
        assert_eq!(root.direct_child_count, 5);
        assert_eq!(root.subtree_size, 150);
        assert_eq!(root.leaf_count, 124);
        assert_eq!(root.max_relative_depth, 25);
        assert_eq!(root.max_width, 121);

        let wide = store
            .tree_statistics(fixture.wide_parent_id)
            .expect("wide stats");
        assert_eq!((wide.direct_child_count, wide.subtree_size), (120, 121));
        assert_eq!(
            (wide.leaf_count, wide.max_relative_depth, wide.max_width),
            (120, 1, 120)
        );

        let leaf = store
            .tree_statistics(fixture.wide_child_ids[0])
            .expect("leaf stats");
        assert_eq!(
            (
                leaf.direct_child_count,
                leaf.subtree_size,
                leaf.leaf_count,
                leaf.max_relative_depth,
                leaf.max_width,
            ),
            (0, 1, 1, 0, 1)
        );

        assert!(
            store
                .contains_ancestor(fixture.root_id, fixture.deep_leaf_id)
                .expect("root contains deep leaf")
                .contains
        );
        assert!(
            store
                .contains_ancestor(fixture.deep_leaf_id, fixture.deep_leaf_id)
                .expect("self containment")
                .contains
        );
        assert!(
            !store
                .contains_ancestor(fixture.deep_leaf_id, fixture.root_id)
                .expect("reverse containment")
                .contains
        );
        assert_eq!(
            store
                .lowest_common_ancestor(fixture.wide_child_ids[0], fixture.wide_child_ids[119])
                .expect("wide LCA")
                .id,
            fixture.wide_parent_id
        );
        assert_eq!(
            store
                .lowest_common_ancestor(fixture.wide_child_ids[0], fixture.deep_leaf_id)
                .expect("branch LCA")
                .id,
            fixture.root_id
        );
        let route = store
            .path_between(fixture.wide_child_ids[0], fixture.deep_leaf_id)
            .expect("cross-branch route");
        assert_eq!(route.distance, 27);
        assert_eq!(
            route.nodes.first().expect("origin").id,
            fixture.wide_child_ids[0]
        );
        assert_eq!(route.nodes[2].id, fixture.root_id);
        assert_eq!(
            route.nodes.last().expect("destination").id,
            fixture.deep_leaf_id
        );
        assert_eq!(
            route.nodes.len(),
            usize::try_from(route.distance).expect("distance") + 1
        );
        let same = store
            .path_between(fixture.root_id, fixture.root_id)
            .expect("same-node route");
        assert_eq!(same.distance, 0);
        assert_eq!(
            same.nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
            vec![fixture.root_id]
        );

        let limit = PageLimit::new(37).expect("limit");
        let mut cursor = None;
        let mut leaves = Vec::new();
        loop {
            let page = store
                .filtered_subtree_page(
                    fixture.root_id,
                    mdtree_core::StructuralPredicate::Leaf,
                    None,
                    limit,
                    cursor.as_ref(),
                )
                .expect("leaf page");
            leaves.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(leaves.len(), 124);
        assert!(leaves
            .windows(2)
            .all(|pair| pair[0].depth <= 25 && pair[1].depth <= 25));
        let internal = store
            .filtered_subtree_page(
                fixture.root_id,
                mdtree_core::StructuralPredicate::Internal,
                Some(1),
                PageLimit::new(10).expect("limit"),
                None,
            )
            .expect("bounded internal page");
        assert_eq!(
            internal
                .items
                .iter()
                .map(|item| item.node.id)
                .collect::<Vec<_>>(),
            vec![
                fixture.root_id,
                fixture.wide_parent_id,
                fixture.deep_parent_id
            ]
        );

        let first = store
            .child_at(fixture.wide_parent_id, 0)
            .expect("first indexed child");
        assert_eq!(
            first.node.as_ref().expect("first child").id,
            fixture.wide_child_ids[0]
        );
        let last = store
            .child_at(fixture.wide_parent_id, 119)
            .expect("last indexed child");
        assert_eq!(
            last.node.as_ref().expect("last child").id,
            fixture.wide_child_ids[119]
        );
        assert!(store
            .child_at(fixture.wide_parent_id, 120)
            .expect("out-of-range indexed child")
            .node
            .is_none());

        let first_neighbors = store
            .adjacent_siblings(fixture.wide_child_ids[0])
            .expect("first neighbors");
        assert!(first_neighbors.previous.is_none());
        assert_eq!(
            first_neighbors.next.as_ref().expect("next sibling").id,
            fixture.wide_child_ids[1]
        );
        let middle_neighbors = store
            .adjacent_siblings(fixture.wide_child_ids[63])
            .expect("middle neighbors");
        assert_eq!(
            middle_neighbors
                .previous
                .as_ref()
                .expect("previous sibling")
                .id,
            fixture.wide_child_ids[62]
        );
        assert_eq!(
            middle_neighbors.next.as_ref().expect("next sibling").id,
            fixture.wide_child_ids[64]
        );
        let last_neighbors = store
            .adjacent_siblings(fixture.wide_child_ids[119])
            .expect("last neighbors");
        assert_eq!(
            last_neighbors
                .previous
                .as_ref()
                .expect("previous sibling")
                .id,
            fixture.wide_child_ids[118]
        );
        assert!(last_neighbors.next.is_none());
        let root_neighbors = store
            .adjacent_siblings(fixture.root_id)
            .expect("root neighbors");
        assert!(root_neighbors.previous.is_none());
        assert!(root_neighbors.next.is_none());

        for id in &fixture.wide_child_ids[..2] {
            store
                .connection()
                .execute(
                    "UPDATE nodes SET sibling_order=0 WHERE id=?1",
                    [id.to_string()],
                )
                .expect("create sibling-order tie");
        }
        let mut tied_ids = fixture.wide_child_ids[..2].to_vec();
        tied_ids.sort_unstable();
        assert_eq!(
            store
                .child_at(fixture.wide_parent_id, 0)
                .expect("tied first child")
                .node
                .expect("tied first child node")
                .id,
            tied_ids[0]
        );
        let tied_neighbors = store
            .adjacent_siblings(tied_ids[0])
            .expect("tied neighbors");
        assert_eq!(
            tied_neighbors.next.expect("ID tie-break next sibling").id,
            tied_ids[1]
        );
    }

    fn assert_canonical_wide_order(nodes: &[NodeProjection], expected_ids: &[mdtree_core::NodeId]) {
        assert_eq!(
            nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
            expected_ids
        );
        assert!(nodes
            .windows(2)
            .all(|pair| (pair[0].sibling_order, pair[0].id) < (pair[1].sibling_order, pair[1].id)));
        assert!(nodes.windows(2).any(|pair| pair[0].id > pair[1].id));
    }

    fn assert_targeted_read(
        observation: &crate::test_support::ReadQueryObservation,
        max_selects: u64,
    ) {
        assert!(
            observation.select_statements <= max_selects,
            "expected at most {max_selects} SELECT authorizations: {observation:?}"
        );
        assert_eq!(observation.write_authorizations, 0);
        assert_eq!(observation.table_read_count("node_versions"), 0);
        assert_eq!(observation.table_read_count("references"), 0);
        assert!(observation.table_reads.keys().all(|table| table == "nodes"));
        assert!(!observation.has_complete_snapshot_signature());
    }
}
