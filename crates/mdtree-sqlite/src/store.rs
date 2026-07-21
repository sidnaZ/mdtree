//! Canonical reads, tree traversal, paths, and atomic mutations.
//!
//! Public fallible operations consistently return [`StoreError`], so method
//! documentation focuses on operation-specific behavior.

#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use mdtree_core::{
    generate_slug, hash_content, hash_revision, Breadcrumb, CloneSubtreeRequest,
    CloneSubtreeResult, CursorScope, Node, NodeFields, NodeHash, NodeId, NodeMetadata,
    NodeRevision, NodeSelector, Page, PageCursor, PageLimit, PagePosition, PaginationError,
    Reference, ReferenceOrigin, ReferenceTarget, ReferenceType, RevisionHashInput, Slug,
    UlidGenerator,
};
use mdtree_markdown::DerivedNodeRecords;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mutation_assembly::{prepare_node_mutation, NodeMutationDraft, PreparedNodeMutation};

/// Canonical storage operation failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Requested node does not exist.
    #[error("node not found: {0}")]
    NotFound(String),
    /// Selector is not unique.
    #[error("ambiguous node selector: {0}")]
    Ambiguous(String),
    /// Optimistic version check failed.
    #[error("version conflict for {node_id}: expected {expected}, found {actual}")]
    Conflict {
        /// Conflicting node.
        node_id: NodeId,
        /// Caller-observed version.
        expected: u64,
        /// Persisted version.
        actual: u64,
    },
    /// An operation ID was reused with a different tool or payload.
    #[error("operation ID was already used with a different mutation: {0}")]
    IdempotencyConflict(String),
    /// Tree invariant would be violated.
    #[error("tree invariant violation: {0}")]
    Invariant(String),
    /// Persisted value cannot form a domain object.
    #[error("invalid persisted node: {0}")]
    InvalidData(String),
    /// Mandatory response fields cannot fit a requested hard byte budget.
    #[error("response budget {requested} bytes is below mandatory minimum {minimum}")]
    BudgetExceeded {
        /// Smallest serialized mandatory response.
        minimum: usize,
        /// Caller-requested hard limit.
        requested: usize,
    },
    /// `SQLite` failure.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Metadata JSON failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Traversal node and relative depth.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeDepth {
    /// Canonical node.
    pub node: Node,
    /// Relative traversal depth.
    pub depth: u32,
}

/// Subtree deletion impact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RemovalImpact {
    /// Affected canonical nodes.
    pub node_count: u64,
    /// External backlinks that become unresolved.
    pub incoming_reference_count: u64,
    /// Whether deletion was executed.
    pub deleted: bool,
}

/// Impact of discarding every retained revision except each canonical head.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryPruneReport {
    /// Canonical nodes whose current revisions are retained.
    pub node_count: u64,
    /// Retained revisions before pruning.
    pub revisions_before: u64,
    /// Historical revisions deleted by the operation.
    pub revisions_removed: u64,
    /// Current head revisions remaining after pruning.
    pub revisions_retained: u64,
}

/// Result of a versioned canonical mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationOutcome {
    /// Canonical state and derived rows were updated and a revision was appended.
    Applied,
    /// Requested semantic state already matches the current revision hash.
    NoOp,
}

/// Deterministic reference-target resolution result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceResolution {
    /// One node and optional section anchor resolved uniquely.
    Resolved {
        /// Stable target node.
        node_id: NodeId,
        /// Verified requested anchor.
        anchor: Option<String>,
    },
    /// Multiple title/alias candidates remain.
    Ambiguous {
        /// Stable deterministic candidate identities.
        candidates: Vec<NodeId>,
    },
    /// No valid target resolved.
    Unresolved,
}

/// One precise workspace-integrity violation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IntegrityFinding {
    /// Stable machine-readable finding code.
    pub code: &'static str,
    /// Affected node when applicable.
    pub node_id: Option<NodeId>,
    /// Actionable detail.
    pub detail: String,
}

/// Complete non-mutating workspace validation result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IntegrityReport {
    /// All detected violations.
    pub findings: Vec<IntegrityFinding>,
}

impl IntegrityReport {
    /// Whether no violations were found.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Likely duplicate-node diagnostic group.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DuplicateCandidate {
    /// Stable candidate IDs.
    pub node_ids: Vec<NodeId>,
    /// Signal that produced the diagnostic.
    pub reason: String,
    /// Current breadcrumbs corresponding to the candidate IDs.
    pub breadcrumbs: Vec<Breadcrumb>,
}

/// Unresolved reference with structural source context.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UnresolvedDiagnostic {
    /// Retained unresolved relationship.
    pub reference: Reference,
    /// Current source-node breadcrumb.
    pub source_breadcrumb: Breadcrumb,
}

/// SQLite-backed canonical store.
pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    /// Wraps a configured, migrated connection.
    #[must_use]
    pub const fn new(connection: Connection) -> Self {
        Self { connection }
    }

    /// Opens a validated workspace.
    ///
    /// # Errors
    ///
    /// Returns [`crate::WorkspaceError`] when opening or validation fails.
    pub fn open(path: &Path) -> Result<Self, crate::WorkspaceError> {
        crate::open_workspace(path).map(Self::new)
    }

    /// Borrows the underlying connection.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// Returns a prior response for the same operation and payload.
    pub fn mutation_receipt(
        &self,
        operation_id: &str,
        tool_name: &str,
        payload_hash: &[u8; 32],
    ) -> Result<Option<String>, StoreError> {
        let receipt = self
            .connection
            .query_row(
                "SELECT tool_name,payload_hash,result_json FROM mutation_receipts WHERE operation_id=?1",
                [operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_tool, stored_hash, result)) = receipt else {
            return Ok(None);
        };
        if stored_tool != tool_name || stored_hash.as_slice() != payload_hash {
            return Err(StoreError::IdempotencyConflict(operation_id.into()));
        }
        Ok(Some(result))
    }

    /// Persists a completed mutation response for safe retry replay.
    pub fn record_mutation_receipt(
        &self,
        operation_id: &str,
        tool_name: &str,
        payload_hash: &[u8; 32],
        result_json: &str,
        created_at: u64,
    ) -> Result<(), StoreError> {
        let created_at = i64::try_from(created_at).map_err(|_| {
            StoreError::InvalidData("receipt timestamp exceeds SQLite range".into())
        })?;
        self.connection.execute(
            "INSERT INTO mutation_receipts(operation_id,tool_name,payload_hash,result_json,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![operation_id, tool_name, payload_hash.as_slice(), result_json, created_at],
        )?;
        Ok(())
    }

    /// Loads by stable identity.
    pub fn get(&self, id: NodeId) -> Result<Option<Node>, StoreError> {
        query_nodes(
            &self.connection,
            &format!("{SELECT_NODE} WHERE n.id=?1"),
            [id.to_string()],
        )
        .map(|mut nodes| nodes.pop())
    }

    /// Resolves an ID, unambiguous slug, or root-relative path.
    pub fn resolve(&self, selector: &NodeSelector) -> Result<Option<Node>, StoreError> {
        match selector {
            NodeSelector::Id(id) => self.get(*id),
            NodeSelector::Slug(slug) => {
                let nodes = query_nodes(
                    &self.connection,
                    &format!("{SELECT_NODE} WHERE n.slug=?1 ORDER BY n.id"),
                    [slug.as_str()],
                )?;
                match nodes.len() {
                    0 => Ok(None),
                    1 => Ok(nodes.into_iter().next()),
                    _ => Err(StoreError::Ambiguous(slug.to_string())),
                }
            }
            NodeSelector::Path(path) => self.resolve_path(path),
        }
    }

    /// Returns the root.
    pub fn root(&self) -> Result<Node, StoreError> {
        query_nodes(
            &self.connection,
            &format!("{SELECT_NODE} WHERE n.parent_id IS NULL"),
            [],
        )?
        .into_iter()
        .next()
        .ok_or_else(|| StoreError::NotFound("root".into()))
    }

    /// Returns the workspace-level revision counter.
    ///
    /// Incremented by database triggers on every persisted node or reference
    /// change, regardless of which process or interface made it, so any
    /// process holding this workspace open can detect that it changed
    /// without polling individual nodes.
    pub fn workspace_revision(&self) -> Result<u64, StoreError> {
        let value: i64 = self.connection.query_row(
            "SELECT revision FROM workspace WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(value).unwrap_or(0))
    }

    /// Reports the impact of retaining only each canonical node's current revision.
    pub fn plan_history_prune(&self) -> Result<HistoryPruneReport, StoreError> {
        history_prune_report(&self.connection)
    }

    /// Atomically discards every retained revision except each canonical head.
    ///
    /// The operation refuses to run when a node has no matching latest revision,
    /// and advances the workspace revision when rows are removed so outstanding
    /// pagination cursors cannot observe a changed history.
    pub fn prune_history(&mut self) -> Result<HistoryPruneReport, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let report = history_prune_report(&transaction)?;
        let removed = transaction.execute(
            "DELETE FROM node_versions AS revision
             WHERE NOT EXISTS (
                 SELECT 1 FROM nodes AS node
                 WHERE node.id=revision.node_id
                   AND node.content_version=revision.version
                   AND node.revision_hash=revision.revision_hash
             )",
            [],
        )?;
        let removed = u64::try_from(removed)
            .map_err(|_| StoreError::InvalidData("revision removal count".into()))?;
        if removed != report.revisions_removed {
            return Err(StoreError::Invariant(
                "history changed while pruning revisions".into(),
            ));
        }
        let retained: i64 =
            transaction.query_row("SELECT COUNT(*) FROM node_versions", [], |row| row.get(0))?;
        if nonnegative(retained)? != report.node_count {
            return Err(StoreError::Invariant(
                "history pruning did not retain exactly one head per node".into(),
            ));
        }
        if removed > 0 {
            transaction.execute(
                "UPDATE workspace SET revision=revision+1 WHERE singleton=1",
                [],
            )?;
        }
        transaction.commit()?;
        Ok(report)
    }

    /// Checkpoints the write-ahead log and rebuilds the database to reclaim space.
    pub fn vacuum(&self) -> Result<(), StoreError> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM")?;
        Ok(())
    }

    /// Returns the parent.
    pub fn parent(&self, id: NodeId) -> Result<Option<Node>, StoreError> {
        query_nodes(
            &self.connection,
            &format!("{SELECT_NODE} JOIN nodes c ON c.parent_id=n.id WHERE c.id=?1"),
            [id.to_string()],
        )
        .map(|mut nodes| nodes.pop())
    }

    /// Returns children in stable sibling order.
    pub fn children(&self, id: NodeId) -> Result<Vec<Node>, StoreError> {
        query_nodes(
            &self.connection,
            &format!("{SELECT_NODE} WHERE n.parent_id=?1 ORDER BY n.sibling_order,n.id"),
            [id.to_string()],
        )
    }

    /// Returns ancestors root-first with distance from the selected node.
    pub fn ancestors(&self, id: NodeId) -> Result<Vec<NodeDepth>, StoreError> {
        self.node_depths(
            &format!(
                "WITH RECURSIVE x(id,depth) AS (
                SELECT parent_id,1 FROM nodes WHERE id=?1 AND parent_id IS NOT NULL
                UNION ALL SELECT n.parent_id,x.depth+1 FROM nodes n JOIN x ON n.id=x.id
                WHERE n.parent_id IS NOT NULL
             ) SELECT {NODE_COLUMNS},x.depth FROM x JOIN nodes n ON n.id=x.id
             ORDER BY x.depth DESC"
            ),
            [id.to_string()],
        )
    }

    /// Returns descendants in stable depth-first order.
    pub fn descendants(&self, id: NodeId) -> Result<Vec<NodeDepth>, StoreError> {
        self.traversal(id, false)
    }

    /// Returns the node plus all descendants.
    pub fn subtree(&self, id: NodeId) -> Result<Vec<NodeDepth>, StoreError> {
        self.traversal(id, true)
    }

    fn traversal(&self, id: NodeId, include_root: bool) -> Result<Vec<NodeDepth>, StoreError> {
        let root_filter = if include_root { "" } else { "WHERE x.depth>0" };
        self.node_depths(
            &format!(
                "WITH RECURSIVE x(id,depth,ordering) AS (
                SELECT id,0,printf('%010d-%s',sibling_order,id) FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,x.depth+1,x.ordering||'/'||printf('%010d-%s',n.sibling_order,n.id)
                FROM nodes n JOIN x ON n.parent_id=x.id
             ) SELECT {NODE_COLUMNS},x.depth FROM x JOIN nodes n ON n.id=x.id
             {root_filter} ORDER BY x.ordering"
            ),
            [id.to_string()],
        )
    }

    /// Returns siblings including the selected node.
    pub fn siblings(&self, id: NodeId) -> Result<Vec<Node>, StoreError> {
        let node = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        node.parent_id()
            .map_or_else(|| Ok(vec![node]), |parent| self.children(parent))
    }

    /// Builds the current display breadcrumb.
    pub fn breadcrumb(&self, id: NodeId) -> Result<Breadcrumb, StoreError> {
        let mut titles: Vec<_> = self
            .ancestors(id)?
            .into_iter()
            .map(|item| item.node.fields().metadata.title.clone())
            .collect();
        titles.push(
            self.get(id)?
                .ok_or_else(|| StoreError::NotFound(id.to_string()))?
                .fields()
                .metadata
                .title
                .clone(),
        );
        Breadcrumb::new(titles).map_err(|error| StoreError::InvalidData(error.to_string()))
    }

    /// Builds the current root-to-node slug path.
    pub fn canonical_path(&self, id: NodeId) -> Result<Vec<Slug>, StoreError> {
        let mut slugs: Vec<_> = self
            .ancestors(id)?
            .into_iter()
            .map(|item| item.node.fields().slug.clone())
            .collect();
        slugs.push(
            self.get(id)?
                .ok_or_else(|| StoreError::NotFound(id.to_string()))?
                .fields()
                .slug
                .clone(),
        );
        Ok(slugs)
    }

    /// Chooses a unique slug and next deterministic order below a parent.
    pub fn next_child_placement(
        &self,
        parent: NodeId,
        title: &str,
    ) -> Result<(Slug, u32), StoreError> {
        if self.get(parent)?.is_none() {
            return Err(StoreError::NotFound(parent.to_string()));
        }
        let children = self.children(parent)?;
        let slug = generate_slug(title, children.iter().map(|node| &node.fields().slug));
        let order = children
            .iter()
            .map(|node| node.fields().sibling_order)
            .max()
            .map_or(0, |order| order.saturating_add(1));
        Ok((slug, order))
    }

    /// Lists immutable revisions in ascending version order.
    pub fn revisions(&self, id: NodeId) -> Result<Vec<NodeRevision>, StoreError> {
        query_revisions(
            &self.connection,
            "SELECT node_id,version,parent_id,slug,markdown_content,sibling_order,metadata_json,
             content_hash,revision_hash,change_summary,created_by,created_at
             FROM node_versions WHERE node_id=?1 ORDER BY version",
            params![id.to_string()],
        )
    }

    /// Loads one exact immutable revision.
    pub fn revision(&self, id: NodeId, version: u64) -> Result<Option<NodeRevision>, StoreError> {
        query_revisions(
            &self.connection,
            "SELECT node_id,version,parent_id,slug,markdown_content,sibling_order,metadata_json,
             content_hash,revision_hash,change_summary,created_by,created_at
             FROM node_versions WHERE node_id=?1 AND version=?2",
            params![id.to_string(), integer(version)?],
        )
        .map(|mut revisions| revisions.pop())
    }

    /// Resolves target text by ID, path, then title/alias, validating an optional anchor.
    pub fn resolve_reference(&self, target_ref: &str) -> Result<ReferenceResolution, StoreError> {
        let (target, anchor) = target_ref
            .split_once('#')
            .map_or((target_ref, None), |(target, anchor)| {
                (target, Some(anchor))
            });
        let direct = NodeId::from_str(target)
            .ok()
            .map(|id| self.get(id))
            .transpose()?
            .flatten();
        let path = if direct.is_none() && target.contains('/') {
            NodeSelector::from_str(target)
                .ok()
                .map(|selector| self.resolve(&selector))
                .transpose()?
                .flatten()
        } else {
            None
        };
        let mut candidates = if direct.is_none() && path.is_none() {
            query_nodes(
                &self.connection,
                &format!(
                    "{SELECT_NODE} WHERE lower(n.title)=lower(?1) OR EXISTS(
                     SELECT 1 FROM json_each(n.metadata_json,'$.aliases')
                     WHERE lower(value)=lower(?1)) ORDER BY n.id"
                ),
                [target],
            )?
        } else {
            Vec::new()
        };
        if candidates.len() > 1 {
            return Ok(ReferenceResolution::Ambiguous {
                candidates: candidates.into_iter().map(|node| node.id()).collect(),
            });
        }
        let Some(node) = direct.or(path).or_else(|| candidates.pop()) else {
            return Ok(ReferenceResolution::Unresolved);
        };
        if let Some(anchor) = anchor {
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sections WHERE node_id=?1 AND anchor=?2)",
                params![node.id().to_string(), anchor],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(ReferenceResolution::Unresolved);
            }
        }
        Ok(ReferenceResolution::Resolved {
            node_id: node.id(),
            anchor: anchor.map(str::to_owned),
        })
    }

    /// Returns typed outgoing references.
    pub fn outgoing_references(&self, id: NodeId) -> Result<Vec<Reference>, StoreError> {
        let sql = REFERENCE_SELECT.to_owned() + " WHERE source_node_id=?1 ORDER BY id";
        query_references(&self.connection, &sql, [id.to_string()])
    }

    /// Returns typed backlinks to a resolved node.
    pub fn backlinks(&self, id: NodeId) -> Result<Vec<Reference>, StoreError> {
        let sql = REFERENCE_SELECT.to_owned() + " WHERE target_node_id=?1 ORDER BY id";
        query_references(&self.connection, &sql, [id.to_string()])
    }

    /// Returns one resumable page of outgoing references in persisted reference order.
    pub fn outgoing_references_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<Reference>, crate::PageReadError> {
        self.reference_page("references", "source_node_id", id, limit, cursor)
    }

    /// Returns one resumable page of backlinks in persisted reference order.
    pub fn backlinks_page(
        &self,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<Reference>, crate::PageReadError> {
        self.reference_page("backlinks", "target_node_id", id, limit, cursor)
    }

    fn reference_page(
        &self,
        operation: &str,
        filter_column: &str,
        id: NodeId,
        limit: PageLimit,
        cursor: Option<&PageCursor>,
    ) -> Result<Page<Reference>, crate::PageReadError> {
        let revision = self.workspace_revision()?;
        let scope = CursorScope::new(operation, Some(id), "canonical_reference_id_order")?;
        let after = match cursor
            .map(|value| value.resume(&scope, revision))
            .transpose()?
        {
            None => 0,
            Some(PagePosition::Reference { row_id }) => row_id,
            Some(_) => return Err(PaginationError::InvalidCursorPosition.into()),
        };
        let sql = format!(
            "SELECT id,source_node_id,target_node_id,target_ref,target_anchor,reference_type,source_section_id,origin,metadata_json
             FROM \"references\" WHERE {filter_column}=?1 AND id>?2 ORDER BY id LIMIT ?3"
        );
        let row_limit = i64::from(limit.get()) + 1;
        let after = i64::try_from(after)
            .map_err(|_| StoreError::InvalidData("reference cursor position".into()))?;
        let mut statement = self.connection.prepare(&sql).map_err(StoreError::from)?;
        let rows = statement
            .query_map(params![id.to_string(), after, row_limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })
            .map_err(StoreError::from)?;
        let mut items = rows
            .map(|row| {
                let (
                    row_id,
                    source,
                    target_id,
                    target_ref,
                    anchor,
                    kind,
                    section,
                    origin,
                    metadata,
                ) = row?;
                let target = if let Some(target_id) = target_id {
                    ReferenceTarget::Resolved {
                        node_id: NodeId::from_str(&target_id)
                            .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                        target_ref,
                        anchor,
                    }
                } else {
                    ReferenceTarget::Unresolved {
                        target_ref: target_ref
                            .ok_or_else(|| StoreError::InvalidData("reference target".into()))?,
                    }
                };
                Ok((
                    u64::try_from(row_id)
                        .map_err(|_| StoreError::InvalidData("reference row ID".into()))?,
                    Reference {
                        source_node_id: NodeId::from_str(&source)
                            .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                        source_section_id: section
                            .as_deref()
                            .map(NodeId::from_str)
                            .transpose()
                            .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                        reference_type: ReferenceType::from_str(&kind)
                            .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                        target,
                        origin: parse_origin(&origin)?,
                        metadata: serde_json::from_str(&metadata)?,
                    },
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let has_more = items.len() > usize::try_from(limit.get()).unwrap_or(usize::MAX);
        if has_more {
            items.pop();
        }
        let next_cursor = if has_more {
            let row_id = items.last().expect("non-empty bounded reference page").0;
            Some(PageCursor::issue(
                revision,
                scope,
                PagePosition::Reference { row_id },
            )?)
        } else {
            None
        };
        Ok(Page::new(
            items.into_iter().map(|(_, reference)| reference).collect(),
            next_cursor,
        ))
    }

    /// Distinct outgoing reference-type names across the entire workspace,
    /// alphabetically — every relation type in use, not just the ones a
    /// caller has discovered so far by loading specific nodes.
    pub fn all_relation_types(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT reference_type FROM \"references\" ORDER BY reference_type",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut types = Vec::new();
        for row in rows {
            types.push(row?);
        }
        Ok(types)
    }

    /// Replaces explicit references atomically with the corresponding node revision.
    pub fn set_explicit_references(
        &mut self,
        change: NodeChange<'_>,
        references: &[Reference],
    ) -> Result<MutationOutcome, StoreError> {
        let current = self
            .get(change.node.id())?
            .ok_or_else(|| StoreError::NotFound(change.node.id().to_string()))?;
        check_version(&current, change.expected_version)?;
        if current.fields().revision_hash == change.node.fields().revision_hash {
            return Ok(MutationOutcome::NoOp);
        }
        let mut keys = std::collections::BTreeSet::new();
        for reference in references {
            if reference.source_node_id != change.node.id()
                || reference.origin != ReferenceOrigin::Explicit
            {
                return Err(StoreError::Invariant(
                    "explicit reference has invalid source or origin".into(),
                ));
            }
            let key = format!(
                "{}:{}",
                reference.reference_type,
                serde_json::to_string(&reference.target)?
            );
            if !keys.insert(key) {
                return Err(StoreError::Invariant("duplicate explicit reference".into()));
            }
        }
        validate_next_revision(change)?;
        let transaction = self.connection.transaction()?;
        update_node(&transaction, change.node, change.expected_version)?;
        insert_revision(&transaction, change.revision)?;
        replace_derived(&transaction, change.node.id(), change.derived)?;
        transaction.execute(
            "DELETE FROM \"references\" WHERE source_node_id=?1 AND origin='explicit'",
            [change.node.id().to_string()],
        )?;
        for reference in references {
            insert_reference(&transaction, reference)?;
        }
        transaction.commit()?;
        Ok(MutationOutcome::Applied)
    }

    /// Validates root, reachability, ordering, hashes, history, derived rows, and FTS consistency.
    pub fn validate_integrity(&self) -> Result<IntegrityReport, StoreError> {
        let mut findings = Vec::new();
        let root_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        if root_count != 1 {
            findings.push(finding(
                "root_count",
                None,
                format!("expected 1 root, found {root_count}"),
            ));
        }
        let orphan_ids=query_ids(&self.connection,"SELECT n.id FROM nodes n LEFT JOIN nodes p ON p.id=n.parent_id WHERE n.parent_id IS NOT NULL AND p.id IS NULL")?;
        for id in orphan_ids {
            findings.push(finding("orphan", Some(id), "parent does not exist".into()));
        }
        let total: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let reachable:i64=self.connection.query_row("WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE parent_id IS NULL UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id) SELECT COUNT(DISTINCT id) FROM x",[],|r|r.get(0))?;
        if reachable != total {
            findings.push(finding(
                "unreachable",
                None,
                format!("{reachable} of {total} nodes reachable"),
            ));
        }
        let bad_orders=query_ids(&self.connection,"SELECT id FROM nodes n WHERE sibling_order != (SELECT COUNT(*) FROM nodes s WHERE s.parent_id IS n.parent_id AND (s.sibling_order<n.sibling_order OR (s.sibling_order=n.sibling_order AND s.id<n.id)))")?;
        for id in bad_orders {
            findings.push(finding(
                "sibling_order",
                Some(id),
                "sibling order is not normalized".into(),
            ));
        }
        for node in query_nodes(
            &self.connection,
            &format!("{SELECT_NODE} ORDER BY n.id"),
            [],
        )? {
            if hash_content(&node.fields().markdown_content) != node.fields().content_hash {
                findings.push(finding(
                    "content_hash",
                    Some(node.id()),
                    "content hash mismatch".into(),
                ));
            }
            if canonical_revision_hash(&node)? != node.fields().revision_hash {
                findings.push(finding(
                    "revision_hash",
                    Some(node.id()),
                    "revision hash does not match canonical fields".into(),
                ));
            }
            if let Some(latest) = self.revisions(node.id())?.last() {
                if latest.version != node.fields().version
                    || latest.revision_hash != node.fields().revision_hash
                {
                    findings.push(finding(
                        "revision_head",
                        Some(node.id()),
                        "head does not match latest revision".into(),
                    ));
                }
            }
        }
        let bad_section_ids = query_ids(
            &self.connection,
            "SELECT id FROM sections WHERE length(content_hash)!=32",
        )?;
        for id in bad_section_ids {
            findings.push(finding(
                "section_hash",
                Some(id),
                "invalid section hash".into(),
            ));
        }
        let sections: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM sections", [], |r| r.get(0))?;
        let fts: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM section_fts", [], |r| r.get(0))?;
        if sections != fts {
            findings.push(finding(
                "fts_consistency",
                None,
                format!("{sections} sections but {fts} FTS rows"),
            ));
        }
        Ok(IntegrityReport { findings })
    }

    /// Lists unresolved references without changing canonical facts.
    pub fn unresolved_references(&self) -> Result<Vec<UnresolvedDiagnostic>, StoreError> {
        let sql = REFERENCE_SELECT.to_owned() + " WHERE target_node_id IS NULL ORDER BY id";
        query_references(&self.connection, &sql, [])?
            .into_iter()
            .map(|reference| {
                Ok(UnresolvedDiagnostic {
                    source_breadcrumb: self.breadcrumb(reference.source_node_id)?,
                    reference,
                })
            })
            .collect()
    }

    /// Finds deterministic likely-duplicate groups by normalized title.
    pub fn duplicate_candidates(&self) -> Result<Vec<DuplicateCandidate>, StoreError> {
        let mut statement=self.connection.prepare("SELECT lower(trim(title)),group_concat(id),COUNT(*) FROM nodes GROUP BY lower(trim(title)) HAVING COUNT(*)>1 ORDER BY 1")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (title, ids) = row?;
            let node_ids = ids
                .split(',')
                .map(NodeId::from_str)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StoreError::InvalidData(e.to_string()))?;
            let breadcrumbs = node_ids
                .iter()
                .map(|id| self.breadcrumb(*id))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(DuplicateCandidate {
                node_ids,
                breadcrumbs,
                reason: format!("same normalized title: {title}"),
            })
        })
        .collect()
    }

    /// Rebuilds sections, extracted references, hashes, and FTS from canonical nodes.
    pub fn rebuild_derived(&mut self, ids: &dyn UlidGenerator) -> Result<(), StoreError> {
        let nodes = query_nodes(
            &self.connection,
            &format!("{SELECT_NODE} ORDER BY n.id"),
            [],
        )?;
        let mut rebuilt = Vec::new();
        for node in &nodes {
            rebuilt.push((
                node.id(),
                mdtree_markdown::build_derived_records(node, ids)
                    .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            ));
        }
        let preserved_sql = REFERENCE_SELECT.to_owned()
            + " WHERE origin NOT IN ('markdown','wikilink') ORDER BY id";
        let mut preserved = query_references(&self.connection, &preserved_sql, [])?;
        for reference in &mut preserved {
            reference.source_section_id = None;
        }
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM section_fts", [])?;
        transaction.execute("DELETE FROM \"references\"", [])?;
        transaction.execute("DELETE FROM sections", [])?;
        for (id, derived) in &rebuilt {
            transaction.execute(
                "UPDATE nodes SET content_hash=?2 WHERE id=?1",
                params![id.to_string(), derived.content_hash.as_bytes().as_slice()],
            )?;
            replace_derived(&transaction, *id, derived)?;
        }
        for reference in &preserved {
            insert_reference(&transaction, reference)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically creates canonical, revision, derived, and FTS rows.
    pub fn create_node(
        &mut self,
        node: &Node,
        revision: &NodeRevision,
        derived: &DerivedNodeRecords,
    ) -> Result<(), StoreError> {
        self.create_node_parts(&[(node, revision, derived)])
    }

    /// Creates a parent-first collection of nodes in one transaction.
    pub fn create_nodes(&mut self, nodes: &[PreparedNodeMutation]) -> Result<(), StoreError> {
        let parts = nodes
            .iter()
            .map(|prepared| (&prepared.node, &prepared.revision, &prepared.derived))
            .collect::<Vec<_>>();
        self.create_node_parts(&parts)
    }

    /// Plans or applies unrelated moves and removals in one transaction.
    #[allow(clippy::too_many_lines)]
    pub fn apply_atomic_tree_batch(
        &mut self,
        moves: &[AtomicTreeMove],
        removals: &[AtomicTreeRemoval],
        dry_run: bool,
    ) -> Result<AtomicTreeBatchResult, StoreError> {
        let operation_count = moves.len().saturating_add(removals.len());
        if operation_count == 0 || operation_count > 50 {
            return Err(StoreError::InvalidData(format!(
                "tree batch operation count {operation_count} is outside 1..=50"
            )));
        }
        let mut selected = BTreeSet::new();
        let mut old_parents = BTreeSet::new();
        for change in moves {
            let id = change.prepared.node.id();
            if !selected.insert(id) {
                return Err(StoreError::Invariant(format!(
                    "node appears more than once in tree batch: {id}"
                )));
            }
            let current = self
                .get(id)?
                .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
            if current.is_root() {
                return Err(StoreError::Invariant(
                    "workspace root cannot be moved".into(),
                ));
            }
            check_version(&current, change.expected_version)?;
            if let Some(parent) = current.parent_id() {
                old_parents.insert(parent);
            }
            let destination = change.prepared.node.parent_id().ok_or_else(|| {
                StoreError::Invariant("moved node requires a destination parent".into())
            })?;
            let parent = self
                .get(destination)?
                .ok_or_else(|| StoreError::NotFound(destination.to_string()))?;
            if !parent.fields().metadata.accepts_children.is_empty()
                && change
                    .prepared
                    .node
                    .fields()
                    .metadata
                    .node_type
                    .as_ref()
                    .is_none_or(|kind| !parent.fields().metadata.accepts_children.contains(kind))
            {
                return Err(StoreError::Invariant(
                    "destination parent does not accept moved node type".into(),
                ));
            }
        }
        let mut removal_sets = Vec::new();
        let mut removed_node_count = 0_usize;
        for removal in removals {
            if !selected.insert(removal.node_id) {
                return Err(StoreError::Invariant(format!(
                    "node appears more than once in tree batch: {}",
                    removal.node_id
                )));
            }
            let current = self
                .get(removal.node_id)?
                .ok_or_else(|| StoreError::NotFound(removal.node_id.to_string()))?;
            if current.is_root() {
                return Err(StoreError::Invariant(
                    "workspace root cannot be removed".into(),
                ));
            }
            check_version(&current, removal.expected_version)?;
            if let Some(parent) = current.parent_id() {
                old_parents.insert(parent);
            }
            let ids = self
                .subtree(removal.node_id)?
                .into_iter()
                .map(|row| row.node.id())
                .collect::<BTreeSet<_>>();
            if removal_sets
                .iter()
                .any(|existing: &BTreeSet<NodeId>| !existing.is_disjoint(&ids))
            {
                return Err(StoreError::Invariant(
                    "removal subtrees overlap in tree batch".into(),
                ));
            }
            removed_node_count = removed_node_count.saturating_add(ids.len());
            removal_sets.push(ids);
        }
        for (index, change) in moves.iter().enumerate() {
            let moved_subtree = self
                .subtree(change.prepared.node.id())?
                .into_iter()
                .map(|row| row.node.id())
                .collect::<BTreeSet<_>>();
            if moves.iter().skip(index + 1).any(|other| {
                moved_subtree.contains(&other.prepared.node.id())
                    || self.subtree(other.prepared.node.id()).is_ok_and(|rows| {
                        rows.iter()
                            .any(|row| row.node.id() == change.prepared.node.id())
                    })
            }) {
                return Err(StoreError::Invariant(
                    "move subtrees must be unrelated".into(),
                ));
            }
            let destination = change.prepared.node.parent_id().ok_or_else(|| {
                StoreError::Invariant("moved node requires a destination parent".into())
            })?;
            if removal_sets
                .iter()
                .any(|set| set.contains(&change.prepared.node.id()) || set.contains(&destination))
            {
                return Err(StoreError::Invariant(
                    "moves cannot originate in or target a removed subtree".into(),
                ));
            }
        }
        let root = self.root()?.id();
        let mut parents = self
            .subtree(root)?
            .into_iter()
            .map(|row| (row.node.id(), row.node.parent_id()))
            .collect::<BTreeMap<_, _>>();
        for change in moves {
            parents.insert(change.prepared.node.id(), change.prepared.node.parent_id());
        }
        for change in moves {
            let start = change.prepared.node.id();
            let mut cursor = change.prepared.node.parent_id();
            let mut visited = BTreeSet::new();
            while let Some(id) = cursor {
                if id == start || !visited.insert(id) {
                    return Err(StoreError::Invariant(
                        "tree batch moves would create a cycle".into(),
                    ));
                }
                cursor = parents.get(&id).copied().flatten();
            }
        }
        let result = AtomicTreeBatchResult {
            status: if dry_run { "planned" } else { "applied" }.into(),
            moved_node_ids: moves.iter().map(|item| item.prepared.node.id()).collect(),
            removed_root_ids: removals.iter().map(|item| item.node_id).collect(),
            removed_node_count: u32::try_from(removed_node_count)
                .map_err(|_| StoreError::InvalidData("removed node count".into()))?,
        };
        if dry_run {
            return Ok(result);
        }
        let transaction = self.connection.transaction()?;
        let mut affected_parents = old_parents;
        for change in moves {
            update_node(&transaction, &change.prepared.node, change.expected_version)?;
            insert_revision(&transaction, &change.prepared.revision)?;
            replace_derived(
                &transaction,
                change.prepared.node.id(),
                &change.prepared.derived,
            )?;
            if let Some(parent) = change.prepared.node.parent_id() {
                affected_parents.insert(parent);
            }
        }
        for (removal, ids) in removals.iter().zip(&removal_sets) {
            transaction.execute(
                "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1
                 UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id)
                 UPDATE \"references\" SET target_ref=COALESCE(target_ref,target_node_id),target_node_id=NULL
                 WHERE target_node_id IN x",
                [removal.node_id.to_string()],
            )?;
            for id in ids {
                transaction
                    .execute("DELETE FROM section_fts WHERE node_id=?1", [id.to_string()])?;
            }
            transaction.execute(
                "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1
                 UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id)
                 DELETE FROM nodes WHERE id IN x",
                [removal.node_id.to_string()],
            )?;
        }
        for parent in affected_parents {
            transaction.execute(
                "WITH ordered AS (
                    SELECT id,ROW_NUMBER() OVER (ORDER BY sibling_order,id)-1 AS normalized
                    FROM nodes WHERE parent_id=?1
                 ) UPDATE nodes SET sibling_order=(SELECT normalized FROM ordered WHERE ordered.id=nodes.id)
                 WHERE id IN (SELECT id FROM ordered)",
                [parent.to_string()],
            )?;
            refresh_fts_context(&transaction, parent)?;
        }
        transaction.commit()?;
        Ok(result)
    }

    /// Validates and applies a heterogeneous mutation batch in one transaction.
    #[allow(clippy::too_many_lines)]
    pub fn apply_mutation_batch(
        &mut self,
        operations: &[PreparedBatchOperation],
        dry_run: bool,
    ) -> Result<MutationBatchResult, StoreError> {
        if operations.is_empty() || operations.len() > 50 {
            return Err(StoreError::InvalidData(format!(
                "mutation batch operation count {} is outside 1..=50",
                operations.len()
            )));
        }
        let mut selected = BTreeSet::new();
        let mut planned = BTreeMap::new();
        let mut affected_parents = BTreeSet::new();
        let mut removed_sets = Vec::new();
        for operation in operations {
            let id = operation.node_id();
            if !selected.insert(id) {
                return Err(StoreError::Invariant(format!(
                    "node appears more than once in mutation batch: {id}"
                )));
            }
            match operation {
                PreparedBatchOperation::Create(item) => {
                    if item.node.fields().version != 1
                        || item.revision.version != 1
                        || item.revision.node_id != id
                        || self.get(id)?.is_some()
                    {
                        return Err(StoreError::Invariant(format!(
                            "invalid or existing batch create: {id}"
                        )));
                    }
                    let parent_id = item.node.parent_id().ok_or_else(|| {
                        StoreError::Invariant("batch cannot create a workspace root".into())
                    })?;
                    let parent = if let Some(parent) = planned.get(&parent_id) {
                        parent
                    } else {
                        &self
                            .get(parent_id)?
                            .ok_or_else(|| StoreError::NotFound(parent_id.to_string()))?
                    };
                    validate_parent_accepts(parent, &item.node)?;
                    planned.insert(id, item.node.clone());
                    affected_parents.insert(parent_id);
                }
                PreparedBatchOperation::Replace {
                    prepared,
                    expected_version,
                    ..
                }
                | PreparedBatchOperation::SetReferences {
                    prepared,
                    expected_version,
                    ..
                } => {
                    let current = self
                        .get(id)?
                        .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
                    check_version(&current, *expected_version)?;
                    validate_next_revision(prepared.change(*expected_version))?;
                    if current.is_root() && prepared.node.parent_id() != current.parent_id() {
                        return Err(StoreError::Invariant(
                            "workspace root cannot be moved".into(),
                        ));
                    }
                    if let Some(parent_id) = prepared.node.parent_id() {
                        let parent = self
                            .get(parent_id)?
                            .or_else(|| planned.get(&parent_id).cloned())
                            .ok_or_else(|| StoreError::NotFound(parent_id.to_string()))?;
                        validate_parent_accepts(&parent, &prepared.node)?;
                        if self
                            .subtree(id)?
                            .iter()
                            .any(|row| row.node.id() == parent_id)
                        {
                            return Err(StoreError::Invariant(
                                "batch replacement would create a cycle".into(),
                            ));
                        }
                        affected_parents.insert(parent_id);
                    }
                    if let Some(parent) = current.parent_id() {
                        affected_parents.insert(parent);
                    }
                }
                PreparedBatchOperation::Remove(removal) => {
                    let current = self
                        .get(id)?
                        .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
                    if current.is_root() {
                        return Err(StoreError::Invariant(
                            "workspace root cannot be removed".into(),
                        ));
                    }
                    check_version(&current, removal.expected_version)?;
                    let ids = self
                        .subtree(id)?
                        .into_iter()
                        .map(|row| row.node.id())
                        .collect::<BTreeSet<_>>();
                    if removed_sets
                        .iter()
                        .any(|prior: &BTreeSet<NodeId>| !prior.is_disjoint(&ids))
                    {
                        return Err(StoreError::Invariant(
                            "batch removal subtrees overlap".into(),
                        ));
                    }
                    if let Some(parent) = current.parent_id() {
                        affected_parents.insert(parent);
                    }
                    removed_sets.push(ids);
                }
            }
        }
        for operation in operations {
            if !matches!(operation, PreparedBatchOperation::Remove(_))
                && removed_sets.iter().any(|ids| {
                    ids.contains(&operation.node_id())
                        || operation
                            .parent_id()
                            .is_some_and(|parent| ids.contains(&parent))
                })
            {
                return Err(StoreError::Invariant(
                    "batch operation conflicts with a removed subtree".into(),
                ));
            }
        }
        let result = MutationBatchResult {
            status: if dry_run { "planned" } else { "applied" }.into(),
            operation_count: u32::try_from(operations.len())
                .map_err(|_| StoreError::InvalidData("operation count".into()))?,
            node_ids: operations
                .iter()
                .map(PreparedBatchOperation::node_id)
                .collect(),
        };
        if dry_run {
            return Ok(result);
        }
        let transaction = self.connection.transaction()?;
        for operation in operations {
            match operation {
                PreparedBatchOperation::Create(item) => {
                    insert_node(&transaction, &item.node)?;
                    insert_revision(&transaction, &item.revision)?;
                    replace_derived(&transaction, item.node.id(), &item.derived)?;
                }
                PreparedBatchOperation::Replace {
                    prepared,
                    expected_version,
                    ..
                } => {
                    update_node(&transaction, &prepared.node, *expected_version)?;
                    insert_revision(&transaction, &prepared.revision)?;
                    replace_derived(&transaction, prepared.node.id(), &prepared.derived)?;
                }
                PreparedBatchOperation::SetReferences {
                    prepared,
                    expected_version,
                    references,
                } => {
                    update_node(&transaction, &prepared.node, *expected_version)?;
                    insert_revision(&transaction, &prepared.revision)?;
                    replace_derived(&transaction, prepared.node.id(), &prepared.derived)?;
                    transaction.execute(
                        "DELETE FROM \"references\" WHERE source_node_id=?1 AND origin='explicit'",
                        [prepared.node.id().to_string()],
                    )?;
                    for reference in references {
                        insert_reference(&transaction, reference)?;
                    }
                }
                PreparedBatchOperation::Remove(removal) => {
                    transaction.execute(
                        "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1 UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id) UPDATE \"references\" SET target_ref=COALESCE(target_ref,target_node_id),target_node_id=NULL WHERE target_node_id IN x",
                        [removal.node_id.to_string()],
                    )?;
                    transaction.execute(
                        "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1 UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id) DELETE FROM section_fts WHERE node_id IN x",
                        [removal.node_id.to_string()],
                    )?;
                    transaction.execute(
                        "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1 UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id) DELETE FROM nodes WHERE id IN x",
                        [removal.node_id.to_string()],
                    )?;
                }
            }
        }
        for parent in affected_parents {
            transaction.execute(
                "WITH ordered AS (SELECT id,ROW_NUMBER() OVER (ORDER BY sibling_order,id)-1 AS normalized FROM nodes WHERE parent_id=?1) UPDATE nodes SET sibling_order=(SELECT normalized FROM ordered WHERE ordered.id=nodes.id) WHERE id IN (SELECT id FROM ordered)",
                [parent.to_string()],
            )?;
            refresh_fts_context(&transaction, parent)?;
        }
        transaction.commit()?;
        Ok(result)
    }

    /// Plans or atomically clones one canonical subtree with explicit-reference remapping.
    #[allow(clippy::too_many_lines)]
    pub fn clone_subtree(
        &mut self,
        request: &CloneSubtreeRequest,
        ids: &dyn UlidGenerator,
    ) -> Result<CloneSubtreeResult, StoreError> {
        let source = self
            .get(request.source_id)?
            .ok_or_else(|| StoreError::NotFound(request.source_id.to_string()))?;
        check_version(&source, request.expected_version)?;
        let destination = self
            .get(request.destination_parent_id)?
            .ok_or_else(|| StoreError::NotFound(request.destination_parent_id.to_string()))?;
        let source_rows = self.subtree(request.source_id)?;
        if source_rows.len() > 100 {
            return Err(StoreError::Invariant(
                "subtree clone is limited to 100 nodes".into(),
            ));
        }
        if !destination.fields().metadata.accepts_children.is_empty()
            && source
                .fields()
                .metadata
                .node_type
                .as_ref()
                .is_none_or(|kind| {
                    !destination
                        .fields()
                        .metadata
                        .accepts_children
                        .contains(kind)
                })
        {
            return Err(StoreError::Invariant(
                "destination parent does not accept the cloned root node type".into(),
            ));
        }
        let destination_children = self.children(request.destination_parent_id)?;
        let root_slug = if destination_children
            .iter()
            .any(|child| child.fields().slug == source.fields().slug)
        {
            generate_slug(
                &source.fields().metadata.title,
                destination_children
                    .iter()
                    .map(|child| &child.fields().slug),
            )
        } else {
            source.fields().slug.clone()
        };
        let sibling_order = request
            .sibling_order
            .unwrap_or_else(|| u32::try_from(destination_children.len()).unwrap_or(u32::MAX))
            .min(u32::try_from(destination_children.len()).unwrap_or(u32::MAX));
        let id_map = source_rows
            .iter()
            .map(|row| (row.node.id(), NodeId::new(ids.generate())))
            .collect::<BTreeMap<_, _>>();
        let prepared = source_rows
            .iter()
            .map(|row| {
                let original = &row.node;
                let is_root = original.id() == request.source_id;
                prepare_node_mutation(
                    NodeMutationDraft {
                        id: id_map[&original.id()],
                        parent_id: if is_root {
                            Some(request.destination_parent_id)
                        } else {
                            original.parent_id().map(|parent| id_map[&parent])
                        },
                        slug: if is_root {
                            root_slug.clone()
                        } else {
                            original.fields().slug.clone()
                        },
                        metadata: original.fields().metadata.clone(),
                        markdown_content: original.fields().markdown_content.clone(),
                        sibling_order: if is_root {
                            sibling_order
                        } else {
                            original.fields().sibling_order
                        },
                        version: 1,
                        created_at: request.created_at,
                        updated_at: request.created_at,
                        created_by: request.created_by.clone(),
                        change_summary: request
                            .change_summary
                            .clone()
                            .or_else(|| Some("Clone subtree".into())),
                    },
                    ids,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut explicit_references = Vec::new();
        for row in &source_rows {
            for reference in self.outgoing_references(row.node.id())? {
                if matches!(
                    reference.origin,
                    ReferenceOrigin::Markdown
                        | ReferenceOrigin::Wikilink
                        | ReferenceOrigin::Inferred
                ) {
                    continue;
                }
                let origin = reference.origin;
                let target = match reference.target {
                    ReferenceTarget::Resolved {
                        node_id,
                        target_ref,
                        anchor,
                    } => ReferenceTarget::Resolved {
                        node_id: id_map.get(&node_id).copied().unwrap_or(node_id),
                        target_ref,
                        anchor,
                    },
                    ReferenceTarget::Unresolved { target_ref } => {
                        ReferenceTarget::Unresolved { target_ref }
                    }
                };
                explicit_references.push(Reference {
                    source_node_id: id_map[&reference.source_node_id],
                    source_section_id: None,
                    reference_type: reference.reference_type,
                    target,
                    origin,
                    metadata: reference.metadata,
                });
            }
        }
        let node_count = u32::try_from(prepared.len())
            .map_err(|_| StoreError::InvalidData("clone node count".into()))?;
        let explicit_reference_count = u32::try_from(explicit_references.len())
            .map_err(|_| StoreError::InvalidData("clone reference count".into()))?;
        let result = CloneSubtreeResult {
            status: if request.dry_run {
                "planned"
            } else {
                "applied"
            }
            .into(),
            source_id: request.source_id,
            destination_parent_id: request.destination_parent_id,
            cloned_root_id: (!request.dry_run).then_some(id_map[&request.source_id]),
            root_slug: root_slug.to_string(),
            sibling_order,
            node_count,
            explicit_reference_count,
        };
        if request.dry_run {
            return Ok(result);
        }
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE nodes SET sibling_order=sibling_order+1
             WHERE parent_id=?1 AND sibling_order>=?2",
            params![request.destination_parent_id.to_string(), sibling_order],
        )?;
        for item in &prepared {
            insert_node(&transaction, &item.node)?;
            insert_revision(&transaction, &item.revision)?;
            replace_derived(&transaction, item.node.id(), &item.derived)?;
        }
        for reference in &explicit_references {
            insert_reference(&transaction, reference)?;
        }
        refresh_fts_context(&transaction, request.destination_parent_id)?;
        transaction.commit()?;
        Ok(result)
    }

    fn create_node_parts(
        &mut self,
        nodes: &[(&Node, &NodeRevision, &DerivedNodeRecords)],
    ) -> Result<(), StoreError> {
        if nodes.is_empty() {
            return Err(StoreError::Invariant(
                "node creation requires at least one node".into(),
            ));
        }

        let mut planned: BTreeMap<NodeId, Node> = BTreeMap::new();
        let mut external_parents = BTreeSet::new();
        for &(node, revision, _) in nodes {
            if node.fields().version != 1 || revision.version != 1 || revision.node_id != node.id()
            {
                return Err(StoreError::Invariant(
                    "creation must start at revision 1".into(),
                ));
            }
            if planned.contains_key(&node.id()) || self.get(node.id())?.is_some() {
                return Err(StoreError::Invariant(format!(
                    "node already exists: {}",
                    node.id()
                )));
            }
            let parent_id = node.parent_id().ok_or_else(|| {
                StoreError::Invariant(
                    "workspace root is created during workspace initialization".into(),
                )
            })?;
            let parent = if let Some(parent) = planned.get(&parent_id) {
                parent.clone()
            } else {
                external_parents.insert(parent_id);
                self.get(parent_id)?
                    .ok_or_else(|| StoreError::NotFound(parent_id.to_string()))?
            };
            if !parent.fields().metadata.accepts_children.is_empty()
                && node
                    .fields()
                    .metadata
                    .node_type
                    .as_ref()
                    .is_none_or(|node_type| {
                        !parent
                            .fields()
                            .metadata
                            .accepts_children
                            .contains(node_type)
                    })
            {
                return Err(StoreError::Invariant(
                    "parent does not accept the child's node type".into(),
                ));
            }
            planned.insert(node.id(), node.clone());
        }

        let transaction = self.connection.transaction()?;
        for &(node, _, _) in nodes {
            insert_node(&transaction, node)?;
        }
        for &(_, revision, _) in nodes {
            insert_revision(&transaction, revision)?;
        }
        for &(node, _, derived) in nodes {
            replace_derived(&transaction, node.id(), derived)?;
        }
        for parent in external_parents {
            refresh_fts_context(&transaction, parent)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically updates content/metadata with optimistic concurrency.
    pub fn update_node(&mut self, change: NodeChange<'_>) -> Result<MutationOutcome, StoreError> {
        self.replace_node(change, false)
    }

    /// Atomically renames while preserving identity and history.
    pub fn rename_node(&mut self, change: NodeChange<'_>) -> Result<MutationOutcome, StoreError> {
        self.replace_node(change, false)
    }

    /// Moves a subtree after rejecting root moves and cycles.
    pub fn move_subtree(&mut self, change: NodeChange<'_>) -> Result<MutationOutcome, StoreError> {
        let Some(parent) = change.node.parent_id() else {
            return Err(StoreError::Invariant("root cannot be moved".into()));
        };
        let cycle: bool = self.connection.query_row(
            "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1
             UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id)
             SELECT EXISTS(SELECT 1 FROM x WHERE id=?2)",
            params![change.node.id().to_string(), parent.to_string()],
            |row| row.get(0),
        )?;
        if cycle {
            return Err(StoreError::Invariant(
                "cannot move below the same subtree".into(),
            ));
        }
        self.replace_node(change, false)
    }

    /// Reorders with the same versioned atomic contract.
    pub fn reorder_node(&mut self, change: NodeChange<'_>) -> Result<MutationOutcome, StoreError> {
        let current = self
            .get(change.node.id())?
            .ok_or_else(|| StoreError::NotFound(change.node.id().to_string()))?;
        check_version(&current, change.expected_version)?;
        let Some(parent_id) = current.parent_id() else {
            return Err(StoreError::Invariant("root cannot be reordered".into()));
        };
        if change.node.parent_id() != Some(parent_id) {
            return Err(StoreError::Invariant(
                "reorder cannot change a node's parent".into(),
            ));
        }
        validate_next_revision(change)?;

        let mut siblings = self.children(parent_id)?;
        let current_index = siblings
            .iter()
            .position(|node| node.id() == current.id())
            .ok_or_else(|| StoreError::Invariant("node is missing from its parent".into()))?;
        let requested_index = usize::try_from(change.node.fields().sibling_order)
            .unwrap_or(usize::MAX)
            .min(siblings.len().saturating_sub(1));
        if current_index == requested_index {
            return Ok(MutationOutcome::NoOp);
        }
        if change.node.fields().sibling_order != u32::try_from(requested_index).unwrap_or(u32::MAX)
        {
            return Err(StoreError::Invariant(
                "reorder position exceeds the last sibling index".into(),
            ));
        }

        let moved = siblings.remove(current_index);
        siblings.insert(requested_index, moved);
        let transaction = self.connection.transaction()?;
        for (index, sibling) in siblings.iter().enumerate() {
            let sibling_order = u32::try_from(index)
                .map_err(|_| StoreError::InvalidData("sibling position".into()))?;
            if sibling.id() == change.node.id() {
                update_node(&transaction, change.node, change.expected_version)?;
                insert_revision(&transaction, change.revision)?;
                replace_derived(&transaction, change.node.id(), change.derived)?;
            } else if sibling.fields().sibling_order != sibling_order {
                let (node, revision) = reordered_sibling(sibling, sibling_order, change.revision)?;
                update_node(&transaction, &node, sibling.fields().version)?;
                insert_revision(&transaction, &revision)?;
            }
        }
        transaction.commit()?;
        Ok(MutationOutcome::Applied)
    }

    /// Restores a historical snapshot as a new head revision.
    pub fn restore_version(
        &mut self,
        id: NodeId,
        target_version: u64,
        expected_version: u64,
        created_at: u64,
        created_by: Option<String>,
        ids: &dyn UlidGenerator,
    ) -> Result<MutationOutcome, StoreError> {
        let current = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        check_version(&current, expected_version)?;
        let snapshot = self
            .revision(id, target_version)?
            .ok_or_else(|| StoreError::NotFound(format!("{id} version {target_version}")))?;
        let content_hash = hash_content(&snapshot.markdown_content);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: id,
            parent_id: snapshot.parent_id,
            slug: &snapshot.slug,
            metadata: &snapshot.metadata,
            markdown_content: &snapshot.markdown_content,
            sibling_order: snapshot.sibling_order,
        })
        .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let restored = Node::new(
            NodeFields {
                id,
                slug: snapshot.slug,
                metadata: snapshot.metadata,
                markdown_content: snapshot.markdown_content,
                sibling_order: snapshot.sibling_order,
                version: expected_version + 1,
                content_hash,
                revision_hash,
                created_at: current.fields().created_at,
                updated_at: created_at,
            },
            snapshot.parent_id,
        )
        .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let revision = NodeRevision {
            node_id: id,
            parent_id: restored.parent_id(),
            slug: restored.fields().slug.clone(),
            metadata: restored.fields().metadata.clone(),
            markdown_content: restored.fields().markdown_content.clone(),
            sibling_order: restored.fields().sibling_order,
            version: restored.fields().version,
            content_hash,
            revision_hash,
            change_summary: Some(format!("Restore version {target_version}")),
            created_by,
            created_at,
        };
        let derived = mdtree_markdown::build_derived_records(&restored, ids)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        self.replace_node(
            NodeChange {
                node: &restored,
                expected_version,
                revision: &revision,
                derived: &derived,
            },
            false,
        )
    }

    /// Computes safe-removal impact without mutation.
    pub fn removal_impact(&self, id: NodeId) -> Result<RemovalImpact, StoreError> {
        if self.get(id)?.is_none() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let nodes: i64 = self
            .connection
            .query_row(SUBTREE_COUNT, [id.to_string()], |row| row.get(0))?;
        let incoming: i64 = self.connection.query_row(
            "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1
             UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id)
             SELECT COUNT(*) FROM \"references\" r
             WHERE r.target_node_id IN x AND r.source_node_id NOT IN x",
            [id.to_string()],
            |row| row.get(0),
        )?;
        Ok(RemovalImpact {
            node_count: nonnegative(nodes)?,
            incoming_reference_count: nonnegative(incoming)?,
            deleted: false,
        })
    }

    /// Dry-runs or deletes a non-root subtree with version protection.
    pub fn remove_subtree(
        &mut self,
        id: NodeId,
        expected_version: u64,
        dry_run: bool,
    ) -> Result<RemovalImpact, StoreError> {
        let node = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        if node.is_root() {
            return Err(StoreError::Invariant("root cannot be removed".into()));
        }
        check_version(&node, expected_version)?;
        let mut impact = self.removal_impact(id)?;
        if dry_run {
            return Ok(impact);
        }
        let mut subtree = self.subtree(id)?;
        subtree.sort_by_key(|item| std::cmp::Reverse(item.depth));
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1
             UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id)
             UPDATE \"references\" SET target_ref=COALESCE(target_ref,target_node_id),target_node_id=NULL
             WHERE target_node_id IN x",
            [id.to_string()],
        )?;
        for item in &subtree {
            transaction.execute(
                "DELETE FROM section_fts WHERE node_id=?1",
                [item.node.id().to_string()],
            )?;
        }
        for item in subtree {
            transaction.execute(
                "DELETE FROM nodes WHERE id=?1",
                [item.node.id().to_string()],
            )?;
        }
        transaction.commit()?;
        impact.deleted = true;
        Ok(impact)
    }

    fn node_depths<P: rusqlite::Params>(
        &self,
        sql: &str,
        parameters: P,
    ) -> Result<Vec<NodeDepth>, StoreError> {
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map(parameters, |row| {
            Ok((raw_node(row)?, row.get::<_, u32>(11)?))
        })?;
        rows.map(|row| {
            let (raw, depth) = row?;
            Ok(NodeDepth {
                node: decode_node(raw)?,
                depth,
            })
        })
        .collect()
    }

    fn resolve_path(&self, path: &[Slug]) -> Result<Option<Node>, StoreError> {
        let mut current = self.root()?;
        let mut segments = path.iter();
        if path
            .first()
            .is_some_and(|slug| slug == &current.fields().slug)
        {
            segments.next();
        }
        for slug in segments {
            let next = query_nodes(
                &self.connection,
                &format!("{SELECT_NODE} WHERE n.parent_id=?1 AND n.slug=?2"),
                [current.id().to_string(), slug.to_string()],
            )?
            .into_iter()
            .next();
            let Some(next) = next else { return Ok(None) };
            current = next;
        }
        Ok(Some(current))
    }

    fn replace_node(
        &mut self,
        change: NodeChange<'_>,
        normalize_order: bool,
    ) -> Result<MutationOutcome, StoreError> {
        let current = self
            .get(change.node.id())?
            .ok_or_else(|| StoreError::NotFound(change.node.id().to_string()))?;
        check_version(&current, change.expected_version)?;
        if current.fields().revision_hash == change.node.fields().revision_hash {
            return Ok(MutationOutcome::NoOp);
        }
        if change.node.fields().version != change.expected_version + 1
            || change.revision.version != change.node.fields().version
            || change.revision.node_id != change.node.id()
        {
            return Err(StoreError::Invariant(
                "mutation must create exactly the next revision".into(),
            ));
        }
        let transaction = self.connection.transaction()?;
        update_node(&transaction, change.node, change.expected_version)?;
        insert_revision(&transaction, change.revision)?;
        replace_derived(&transaction, change.node.id(), change.derived)?;
        if normalize_order {
            transaction.execute(
                "WITH ordered AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY sibling_order,id)-1 AS normalized
                    FROM nodes WHERE parent_id IS ?1
                 ) UPDATE nodes SET sibling_order=(
                    SELECT normalized FROM ordered WHERE ordered.id=nodes.id
                 ) WHERE id IN (SELECT id FROM ordered)",
                [change.node.parent_id().map(|id| id.to_string())],
            )?;
        }
        transaction.commit()?;
        Ok(MutationOutcome::Applied)
    }
}

/// Complete precomputed input to one canonical mutation.
#[derive(Clone, Copy)]
pub struct NodeChange<'a> {
    /// New canonical node state.
    pub node: &'a Node,
    /// Version read by the caller.
    pub expected_version: u64,
    /// Immutable new revision.
    pub revision: &'a NodeRevision,
    /// Precomputed derived state.
    pub derived: &'a DerivedNodeRecords,
}

/// One fully prepared move in an atomic tree batch.
#[derive(Clone, Debug)]
pub struct AtomicTreeMove {
    /// Fully assembled new canonical state for the moved root.
    pub prepared: PreparedNodeMutation,
    /// Version observed before planning the move.
    pub expected_version: u64,
}

/// One guarded removal in an atomic tree batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicTreeRemoval {
    /// Root of the subtree to remove.
    pub node_id: NodeId,
    /// Version observed before planning the removal.
    pub expected_version: u64,
}

/// Planned or applied focused tree-batch result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AtomicTreeBatchResult {
    /// `planned` for dry runs and `applied` after commit.
    pub status: String,
    /// Moved roots in request order.
    pub moved_node_ids: Vec<NodeId>,
    /// Removed subtree roots in request order.
    pub removed_root_ids: Vec<NodeId>,
    /// Total nodes covered by all removed subtrees.
    pub removed_node_count: u32,
}

/// One fully planned operation in a heterogeneous atomic mutation batch.
#[derive(Clone, Debug)]
pub enum PreparedBatchOperation {
    /// Create a new non-root node; parent-first ordering is required.
    Create(PreparedNodeMutation),
    /// Replace an existing node for update, rename, move, or reorder semantics.
    Replace {
        /// Fully assembled next revision.
        prepared: PreparedNodeMutation,
        /// Version observed while planning.
        expected_version: u64,
    },
    /// Remove an existing subtree.
    Remove(AtomicTreeRemoval),
    /// Replace the node revision and its explicit reference set.
    SetReferences {
        /// Fully assembled next revision.
        prepared: PreparedNodeMutation,
        /// Version observed while planning.
        expected_version: u64,
        /// Complete desired explicit reference set.
        references: Vec<Reference>,
    },
}

impl PreparedBatchOperation {
    fn node_id(&self) -> NodeId {
        match self {
            Self::Create(item) => item.node.id(),
            Self::Replace { prepared, .. } | Self::SetReferences { prepared, .. } => {
                prepared.node.id()
            }
            Self::Remove(item) => item.node_id,
        }
    }

    fn parent_id(&self) -> Option<NodeId> {
        match self {
            Self::Create(item) => item.node.parent_id(),
            Self::Replace { prepared, .. } | Self::SetReferences { prepared, .. } => {
                prepared.node.parent_id()
            }
            Self::Remove(_) => None,
        }
    }
}

/// Result of a planned or committed heterogeneous mutation batch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MutationBatchResult {
    /// `planned` for a dry run and `applied` after commit.
    pub status: String,
    /// Number of requested operations.
    pub operation_count: u32,
    /// Affected identities in request order.
    pub node_ids: Vec<NodeId>,
}

pub(crate) const SELECT_NODE: &str = "SELECT n.id,n.parent_id,n.slug,n.markdown_content,n.sibling_order,
 n.content_version,n.content_hash,n.revision_hash,n.metadata_json,n.created_at,n.updated_at FROM nodes n";
pub(crate) const NODE_COLUMNS: &str = "n.id,n.parent_id,n.slug,n.markdown_content,n.sibling_order,
 n.content_version,n.content_hash,n.revision_hash,n.metadata_json,n.created_at,n.updated_at";
const REFERENCE_SELECT: &str = "SELECT source_node_id,target_node_id,target_ref,target_anchor,
 reference_type,source_section_id,origin,metadata_json FROM \"references\"";
const SUBTREE_COUNT: &str = "WITH RECURSIVE x(id) AS (SELECT id FROM nodes WHERE id=?1
 UNION ALL SELECT n.id FROM nodes n JOIN x ON n.parent_id=x.id) SELECT COUNT(*) FROM x";

struct RawNode {
    id: String,
    parent: Option<String>,
    slug: String,
    markdown: String,
    order: u32,
    version: i64,
    content_hash: Vec<u8>,
    revision_hash: Vec<u8>,
    metadata: String,
    created: i64,
    updated: i64,
}

fn query_revisions<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<NodeRevision>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, u32>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Vec<u8>>(7)?,
            row.get::<_, Vec<u8>>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, i64>(11)?,
        ))
    })?;
    rows.map(|row| {
        let (
            node_id,
            version,
            parent,
            slug,
            markdown,
            order,
            metadata,
            content_hash,
            revision_hash,
            summary,
            author,
            created,
        ) = row?;
        Ok(NodeRevision {
            node_id: NodeId::from_str(&node_id)
                .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            parent_id: parent
                .as_deref()
                .map(NodeId::from_str)
                .transpose()
                .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            slug: Slug::from_str(&slug).map_err(|e| StoreError::InvalidData(e.to_string()))?,
            metadata: serde_json::from_str(&metadata)?,
            markdown_content: markdown,
            sibling_order: order,
            version: nonnegative(version)?,
            content_hash: digest(&content_hash)?,
            revision_hash: digest(&revision_hash)?,
            change_summary: summary,
            created_by: author,
            created_at: nonnegative(created)?,
        })
    })
    .collect()
}

fn query_references<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<Reference>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    rows.map(|row| {
        let (source, target_id, target_ref, anchor, kind, section, origin_value, metadata) = row?;
        let target = if let Some(target_id) = target_id {
            ReferenceTarget::Resolved {
                node_id: NodeId::from_str(&target_id)
                    .map_err(|e| StoreError::InvalidData(e.to_string()))?,
                target_ref,
                anchor,
            }
        } else {
            ReferenceTarget::Unresolved {
                target_ref: target_ref
                    .ok_or_else(|| StoreError::InvalidData("reference target".into()))?,
            }
        };
        Ok(Reference {
            source_node_id: NodeId::from_str(&source)
                .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            source_section_id: section
                .as_deref()
                .map(NodeId::from_str)
                .transpose()
                .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            reference_type: ReferenceType::from_str(&kind)
                .map_err(|e| StoreError::InvalidData(e.to_string()))?,
            target,
            origin: parse_origin(&origin_value)?,
            metadata: serde_json::from_str(&metadata)?,
        })
    })
    .collect()
}

fn query_ids(connection: &Connection, sql: &str) -> Result<Vec<NodeId>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| NodeId::from_str(&row?).map_err(|e| StoreError::InvalidData(e.to_string())))
        .collect()
}

fn finding(code: &'static str, node_id: Option<NodeId>, detail: String) -> IntegrityFinding {
    IntegrityFinding {
        code,
        node_id,
        detail,
    }
}

pub(crate) fn query_nodes<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<Node>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(parameters, raw_node)?;
    rows.map(|row| decode_node(row?)).collect()
}

pub(crate) fn query_ordered_depths<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<(NodeDepth, String)>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok((
            raw_node(row)?,
            row.get::<_, u32>(11)?,
            row.get::<_, String>(12)?,
        ))
    })?;
    rows.map(|row| {
        let (raw, depth, ordering) = row?;
        Ok((
            NodeDepth {
                node: decode_node(raw)?,
                depth,
            },
            ordering,
        ))
    })
    .collect()
}

fn raw_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawNode> {
    Ok(RawNode {
        id: row.get(0)?,
        parent: row.get(1)?,
        slug: row.get(2)?,
        markdown: row.get(3)?,
        order: row.get(4)?,
        version: row.get(5)?,
        content_hash: row.get(6)?,
        revision_hash: row.get(7)?,
        metadata: row.get(8)?,
        created: row.get(9)?,
        updated: row.get(10)?,
    })
}

fn decode_node(raw: RawNode) -> Result<Node, StoreError> {
    let parse_id =
        |value: &str| NodeId::from_str(value).map_err(|e| StoreError::InvalidData(e.to_string()));
    Node::new(
        NodeFields {
            id: parse_id(&raw.id)?,
            slug: Slug::from_str(&raw.slug).map_err(|e| StoreError::InvalidData(e.to_string()))?,
            metadata: serde_json::from_str::<NodeMetadata>(&raw.metadata)?,
            markdown_content: raw.markdown,
            sibling_order: raw.order,
            version: nonnegative(raw.version)?,
            content_hash: digest(&raw.content_hash)?,
            revision_hash: digest(&raw.revision_hash)?,
            created_at: nonnegative(raw.created)?,
            updated_at: nonnegative(raw.updated)?,
        },
        raw.parent.as_deref().map(parse_id).transpose()?,
    )
    .map_err(|e| StoreError::InvalidData(e.to_string()))
}

fn digest(value: &[u8]) -> Result<NodeHash, StoreError> {
    let bytes: [u8; 32] = value
        .try_into()
        .map_err(|_| StoreError::InvalidData("hash length".into()))?;
    Ok(NodeHash::new(bytes))
}

pub(crate) fn insert_node(tx: &Transaction<'_>, node: &Node) -> Result<(), StoreError> {
    write_node(tx, node, None)?;
    Ok(())
}

fn update_node(tx: &Transaction<'_>, node: &Node, expected: u64) -> Result<(), StoreError> {
    if write_node(tx, node, Some(expected))? == 1 {
        Ok(())
    } else {
        Err(StoreError::Conflict {
            node_id: node.id(),
            expected,
            actual: expected,
        })
    }
}

fn write_node(
    tx: &Transaction<'_>,
    node: &Node,
    expected: Option<u64>,
) -> Result<usize, StoreError> {
    let f = node.fields();
    if let Some(expected) = expected {
        tx.execute("UPDATE nodes SET parent_id=?2,title=?3,slug=?4,summary=?5,node_type=?6,
            markdown_content=?7,sibling_order=?8,content_version=?9,content_hash=?10,revision_hash=?11,
            metadata_json=?12,updated_at=?13 WHERE id=?1 AND content_version=?14", params![
                f.id.to_string(),node.parent_id().map(|id|id.to_string()),&f.metadata.title,f.slug.as_str(),
                f.metadata.summary.as_deref(),f.metadata.node_type.as_ref().map(ToString::to_string),&f.markdown_content,
                f.sibling_order,integer(f.version)?,f.content_hash.as_bytes().as_slice(),f.revision_hash.as_bytes().as_slice(),
                serde_json::to_string(&f.metadata)?,integer(f.updated_at)?,integer(expected)?])
            .map_err(Into::into)
    } else {
        tx.execute("INSERT INTO nodes (id,parent_id,title,slug,summary,node_type,markdown_content,
            sibling_order,content_version,content_hash,revision_hash,metadata_json,created_at,updated_at)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)", params![
                f.id.to_string(),node.parent_id().map(|id|id.to_string()),&f.metadata.title,f.slug.as_str(),
                f.metadata.summary.as_deref(),f.metadata.node_type.as_ref().map(ToString::to_string),&f.markdown_content,
                f.sibling_order,integer(f.version)?,f.content_hash.as_bytes().as_slice(),f.revision_hash.as_bytes().as_slice(),
                serde_json::to_string(&f.metadata)?,integer(f.created_at)?,integer(f.updated_at)?])
            .map_err(Into::into)
    }
}

pub(crate) fn insert_revision(tx: &Transaction<'_>, r: &NodeRevision) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO node_versions (node_id,version,parent_id,title,slug,markdown_content,
        sibling_order,metadata_json,content_hash,revision_hash,change_summary,created_by,created_at)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            r.node_id.to_string(),
            integer(r.version)?,
            r.parent_id.map(|id| id.to_string()),
            &r.metadata.title,
            r.slug.as_str(),
            &r.markdown_content,
            r.sibling_order,
            serde_json::to_string(&r.metadata)?,
            r.content_hash.as_bytes().as_slice(),
            r.revision_hash.as_bytes().as_slice(),
            r.change_summary.as_deref(),
            r.created_by.as_deref(),
            integer(r.created_at)?
        ],
    )?;
    Ok(())
}

pub(crate) fn replace_derived(
    tx: &Transaction<'_>,
    id: NodeId,
    d: &DerivedNodeRecords,
) -> Result<(), StoreError> {
    tx.execute("DELETE FROM section_fts WHERE node_id=?1", [id.to_string()])?;
    tx.execute(
        "DELETE FROM \"references\" WHERE source_node_id=?1 AND origin IN ('markdown','wikilink')",
        [id.to_string()],
    )?;
    tx.execute("DELETE FROM sections WHERE node_id=?1", [id.to_string()])?;
    for s in &d.sections {
        tx.execute("INSERT INTO sections (id,node_id,parent_section_id,heading,heading_level,anchor,
            start_byte,end_byte,content,content_hash,position) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![s.id.to_string(),s.node_id.to_string(),s.parent_section_id.map(|v|v.to_string()),s.heading.as_deref(),
            s.heading_level,s.anchor.as_deref(),integer(s.start_byte)?,integer(s.end_byte)?,&s.content,
            s.content_hash.as_bytes().as_slice(),s.position])?;
    }
    for r in &d.references {
        insert_reference(tx, r)?;
    }
    for f in &d.fts_documents {
        tx.execute("INSERT INTO section_fts (section_id,node_id,title,aliases,summary,heading,content,tags,keywords)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![f.section_id.to_string(),f.node_id.to_string(),
            &f.title,&f.aliases,&f.summary,&f.heading,&f.content,&f.tags,&f.keywords])?;
    }
    refresh_fts_context(tx, id)?;
    Ok(())
}

fn reordered_sibling(
    current: &Node,
    sibling_order: u32,
    cause: &NodeRevision,
) -> Result<(Node, NodeRevision), StoreError> {
    let fields = current.fields();
    let revision_hash = hash_revision(RevisionHashInput {
        node_id: current.id(),
        parent_id: current.parent_id(),
        slug: &fields.slug,
        metadata: &fields.metadata,
        markdown_content: &fields.markdown_content,
        sibling_order,
    })
    .map_err(|error| StoreError::InvalidData(error.to_string()))?;
    let node = Node::new(
        NodeFields {
            id: current.id(),
            slug: fields.slug.clone(),
            metadata: fields.metadata.clone(),
            markdown_content: fields.markdown_content.clone(),
            sibling_order,
            version: fields.version + 1,
            content_hash: fields.content_hash,
            revision_hash,
            created_at: fields.created_at,
            updated_at: cause.created_at,
        },
        current.parent_id(),
    )
    .map_err(|error| StoreError::InvalidData(error.to_string()))?;
    let revision = NodeRevision {
        node_id: node.id(),
        parent_id: node.parent_id(),
        slug: node.fields().slug.clone(),
        metadata: node.fields().metadata.clone(),
        markdown_content: node.fields().markdown_content.clone(),
        sibling_order,
        version: node.fields().version,
        content_hash: node.fields().content_hash,
        revision_hash,
        change_summary: Some(format!(
            "Sibling position adjusted by reorder of {}",
            cause.node_id
        )),
        created_by: cause.created_by.clone(),
        created_at: cause.created_at,
    };
    Ok((node, revision))
}

fn canonical_revision_hash(node: &Node) -> Result<NodeHash, StoreError> {
    let fields = node.fields();
    hash_revision(RevisionHashInput {
        node_id: node.id(),
        parent_id: node.parent_id(),
        slug: &fields.slug,
        metadata: &fields.metadata,
        markdown_content: &fields.markdown_content,
        sibling_order: fields.sibling_order,
    })
    .map_err(|error| StoreError::InvalidData(error.to_string()))
}

fn refresh_fts_context(tx: &Transaction<'_>, id: NodeId) -> Result<(), StoreError> {
    tx.execute("UPDATE section_fts SET
        breadcrumb=COALESCE((WITH RECURSIVE a(id,parent_id,title,summary,depth) AS (
            SELECT id,parent_id,title,summary,0 FROM nodes WHERE id=?1
            UNION ALL SELECT n.id,n.parent_id,n.title,n.summary,a.depth+1 FROM nodes n JOIN a ON a.parent_id=n.id
        ) SELECT group_concat(title,' > ') FROM (SELECT title FROM a ORDER BY depth DESC)),''),
        ancestor_context=COALESCE((WITH RECURSIVE a(id,parent_id,title,summary) AS (
            SELECT id,parent_id,title,summary FROM nodes WHERE id=(SELECT parent_id FROM nodes WHERE id=?1)
            UNION ALL SELECT n.id,n.parent_id,n.title,n.summary FROM nodes n JOIN a ON a.parent_id=n.id
        ) SELECT group_concat(title||' '||COALESCE(summary,''),' ') FROM a),''),
        child_context=COALESCE((SELECT group_concat(title,' ') FROM nodes WHERE parent_id=?1),'')
        WHERE node_id=?1",[id.to_string()])?;
    Ok(())
}

pub(crate) fn insert_reference(tx: &Transaction<'_>, r: &Reference) -> Result<(), StoreError> {
    let (target_id, target_ref, anchor) = match &r.target {
        ReferenceTarget::Resolved {
            node_id,
            target_ref,
            anchor,
        } => (
            Some(node_id.to_string()),
            target_ref.clone(),
            anchor.clone(),
        ),
        ReferenceTarget::Unresolved { target_ref } => (None, Some(target_ref.clone()), None),
    };
    tx.execute(
        "INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,target_anchor,
        reference_type,source_section_id,origin,metadata_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            r.source_node_id.to_string(),
            target_id,
            target_ref,
            anchor,
            r.reference_type.as_str(),
            r.source_section_id.map(|v| v.to_string()),
            origin(r.origin),
            serde_json::to_string(&r.metadata)?
        ],
    )?;
    Ok(())
}

const fn origin(value: ReferenceOrigin) -> &'static str {
    match value {
        ReferenceOrigin::Explicit => "explicit",
        ReferenceOrigin::Markdown => "markdown",
        ReferenceOrigin::Wikilink => "wikilink",
        ReferenceOrigin::ImportedMetadata => "imported_metadata",
        ReferenceOrigin::Inferred => "inferred",
        ReferenceOrigin::Agent => "agent",
    }
}

fn parse_origin(value: &str) -> Result<ReferenceOrigin, StoreError> {
    match value {
        "explicit" => Ok(ReferenceOrigin::Explicit),
        "markdown" => Ok(ReferenceOrigin::Markdown),
        "wikilink" => Ok(ReferenceOrigin::Wikilink),
        "imported_metadata" => Ok(ReferenceOrigin::ImportedMetadata),
        "inferred" => Ok(ReferenceOrigin::Inferred),
        "agent" => Ok(ReferenceOrigin::Agent),
        _ => Err(StoreError::InvalidData(format!("reference origin {value}"))),
    }
}

fn validate_next_revision(change: NodeChange<'_>) -> Result<(), StoreError> {
    if change.node.fields().version == change.expected_version + 1
        && change.revision.version == change.node.fields().version
        && change.revision.node_id == change.node.id()
    {
        Ok(())
    } else {
        Err(StoreError::Invariant(
            "mutation must create exactly the next revision".into(),
        ))
    }
}

fn check_version(node: &Node, expected: u64) -> Result<(), StoreError> {
    let actual = node.fields().version;
    if actual == expected {
        Ok(())
    } else {
        Err(StoreError::Conflict {
            node_id: node.id(),
            expected,
            actual,
        })
    }
}

fn validate_parent_accepts(parent: &Node, child: &Node) -> Result<(), StoreError> {
    if !parent.fields().metadata.accepts_children.is_empty()
        && child
            .fields()
            .metadata
            .node_type
            .as_ref()
            .is_none_or(|kind| !parent.fields().metadata.accepts_children.contains(kind))
    {
        Err(StoreError::Invariant(
            "parent does not accept the child's node type".into(),
        ))
    } else {
        Ok(())
    }
}

fn history_prune_report(connection: &Connection) -> Result<HistoryPruneReport, StoreError> {
    let invalid_heads: i64 = connection.query_row(
        "SELECT COUNT(*) FROM nodes AS node
         WHERE NOT EXISTS (
             SELECT 1 FROM node_versions AS revision
             WHERE revision.node_id=node.id
               AND revision.version=node.content_version
               AND revision.revision_hash=node.revision_hash
         )
         OR node.content_version != (
             SELECT MAX(revision.version) FROM node_versions AS revision
             WHERE revision.node_id=node.id
         )",
        [],
        |row| row.get(0),
    )?;
    if invalid_heads != 0 {
        return Err(StoreError::Invariant(format!(
            "cannot prune history: {invalid_heads} node heads do not match their latest revision"
        )));
    }
    let node_count =
        nonnegative(
            connection.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, i64>(0))?,
        )?;
    let revisions_before = nonnegative(connection.query_row(
        "SELECT COUNT(*) FROM node_versions",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    let revisions_removed = revisions_before
        .checked_sub(node_count)
        .ok_or_else(|| StoreError::Invariant("fewer revisions than canonical nodes".into()))?;
    Ok(HistoryPruneReport {
        node_count,
        revisions_before,
        revisions_removed,
        revisions_retained: node_count,
    })
}

fn integer(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::InvalidData("integer range".into()))
}
fn nonnegative(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::InvalidData("negative integer".into()))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        CursorScope, Node, NodeFields, NodeHash, NodeId, NodeMetadata, NodeRevision, NodeSelector,
        PageCursor, PagePosition, PaginationErrorCode, Reference, ReferenceOrigin, ReferenceTarget,
        ReferenceType, SequentialUlidGenerator, Slug,
    };
    use mdtree_markdown::{build_derived_records, DerivedNodeRecords};
    use rusqlite::params;
    use tempfile::{tempdir, TempDir};

    use super::{
        AtomicTreeMove, AtomicTreeRemoval, MutationOutcome, NodeChange, PreparedBatchOperation,
        ReferenceResolution, SqliteStore, StoreError,
    };
    use crate::{
        create_workspace,
        mutation_assembly::{NodeMutationDraft, PreparedNodeMutation},
        prepare_node_mutation,
    };

    struct Fixture {
        _directory: TempDir,
        path: std::path::PathBuf,
        store: SqliteStore,
    }

    fn id(value: &str) -> NodeId {
        NodeId::from_str(value).expect("fixture ID")
    }

    fn node(
        raw_id: &str,
        parent: Option<NodeId>,
        title: &str,
        slug: &str,
        order: u32,
        version: u64,
    ) -> Node {
        Node::new(
            NodeFields {
                id: id(raw_id),
                slug: Slug::from_str(slug).expect("fixture slug"),
                metadata: NodeMetadata::new(title),
                markdown_content: format!("# {title}\n"),
                sibling_order: order,
                version,
                content_hash: NodeHash::new([u8::try_from(version).expect("test version"); 32]),
                revision_hash: NodeHash::new(
                    [u8::try_from(version + 1).expect("test version"); 32],
                ),
                created_at: 1,
                updated_at: version,
            },
            parent,
        )
        .expect("fixture node")
    }

    fn revision(node: &Node) -> NodeRevision {
        let fields = node.fields();
        NodeRevision {
            node_id: node.id(),
            parent_id: node.parent_id(),
            slug: fields.slug.clone(),
            metadata: fields.metadata.clone(),
            markdown_content: fields.markdown_content.clone(),
            sibling_order: fields.sibling_order,
            version: fields.version,
            content_hash: fields.content_hash,
            revision_hash: fields.revision_hash,
            change_summary: Some("test mutation".into()),
            created_by: Some("test".into()),
            created_at: fields.updated_at,
        }
    }

    fn derived(node: &Node, seed: u64) -> DerivedNodeRecords {
        build_derived_records(node, &SequentialUlidGenerator::new(seed)).expect("derived records")
    }

    fn prepared_reorder(node: &Node, sibling_order: u32, now: u64) -> PreparedNodeMutation {
        let fields = node.fields();
        prepare_node_mutation(
            NodeMutationDraft {
                id: node.id(),
                parent_id: node.parent_id(),
                slug: fields.slug.clone(),
                metadata: fields.metadata.clone(),
                markdown_content: fields.markdown_content.clone(),
                sibling_order,
                version: fields.version + 1,
                created_at: fields.created_at,
                updated_at: now,
                created_by: Some("test".into()),
                change_summary: Some("test reorder".into()),
            },
            &SequentialUlidGenerator::new(now),
        )
        .expect("prepared reorder")
    }

    fn fixture() -> Fixture {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("store.mdtree");
        let root = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XM",
            None,
            "Project",
            "project",
            0,
            1,
        );
        let connection = create_workspace(&path, "Project", &root).expect("workspace");
        Fixture {
            _directory: directory,
            path,
            store: SqliteStore::new(connection),
        }
    }

    fn create(store: &mut SqliteStore, node: &Node, seed: u64) {
        store
            .create_node(node, &revision(node), &derived(node, seed))
            .expect("node creation");
    }

    #[test]
    fn history_pruning_retains_heads_versions_and_invalidates_cursors() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let child = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Child",
            "child",
            0,
            1,
        );
        create(&mut fixture.store, &child, 100);
        let version_two = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Version Two",
            "version-two",
            0,
            2,
        );
        fixture
            .store
            .rename_node(NodeChange {
                node: &version_two,
                expected_version: 1,
                revision: &revision(&version_two),
                derived: &derived(&version_two, 200),
            })
            .expect("second revision");

        let workspace_revision = fixture
            .store
            .workspace_revision()
            .expect("workspace revision");
        let planned = fixture.store.plan_history_prune().expect("prune plan");
        assert_eq!(planned.node_count, 2);
        assert_eq!(planned.revisions_before, 3);
        assert_eq!(planned.revisions_removed, 1);
        assert_eq!(
            fixture.store.revisions(child.id()).expect("history").len(),
            2
        );

        assert_eq!(fixture.store.prune_history().expect("prune"), planned);
        assert_eq!(
            fixture
                .store
                .workspace_revision()
                .expect("workspace revision"),
            workspace_revision + 1
        );
        assert_eq!(
            fixture
                .store
                .revisions(child.id())
                .expect("retained history")
                .iter()
                .map(|revision| revision.version)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert!(fixture
            .store
            .revision(child.id(), 1)
            .expect("old revision lookup")
            .is_none());

        let version_three = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Version Three",
            "version-three",
            0,
            3,
        );
        fixture
            .store
            .rename_node(NodeChange {
                node: &version_three,
                expected_version: 2,
                revision: &revision(&version_three),
                derived: &derived(&version_three, 300),
            })
            .expect("post-prune revision");
        assert_eq!(
            fixture
                .store
                .revisions(child.id())
                .expect("post-prune history")
                .iter()
                .map(|revision| revision.version)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn history_pruning_refuses_a_missing_canonical_head() {
        let mut fixture = fixture();
        fixture
            .store
            .connection()
            .execute("DELETE FROM node_versions", [])
            .expect("remove corrupt head");

        let error = fixture
            .store
            .prune_history()
            .expect_err("corrupt history must be rejected");
        assert!(matches!(error, StoreError::Invariant(_)));
        assert_eq!(
            fixture
                .store
                .connection()
                .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, i64>(0))
                .expect("node count"),
            1
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reads_selectors_traversal_depths_and_paths_on_asymmetric_tree() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let area = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Area",
            "area",
            0,
            1,
        );
        let sibling = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(root.id()),
            "Other",
            "other",
            1,
            1,
        );
        let leaf = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XQ",
            Some(area.id()),
            "Leaf",
            "leaf",
            0,
            1,
        );
        let other_leaf = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XR",
            Some(sibling.id()),
            "Other Leaf",
            "leaf",
            0,
            1,
        );
        create(&mut fixture.store, &area, 100);
        create(&mut fixture.store, &sibling, 200);
        create(&mut fixture.store, &leaf, 300);
        create(&mut fixture.store, &other_leaf, 400);

        assert_eq!(
            fixture.store.get(leaf.id()).expect("read").map(|n| n.id()),
            Some(leaf.id())
        );
        assert_eq!(
            fixture
                .store
                .resolve(&NodeSelector::from_str("area/leaf").expect("selector"))
                .expect("resolve")
                .map(|n| n.id()),
            Some(leaf.id())
        );
        assert!(fixture
            .store
            .resolve(&NodeSelector::from_str("missing").expect("selector"))
            .expect("missing")
            .is_none());
        assert!(matches!(
            fixture
                .store
                .resolve(&NodeSelector::from_str("leaf").expect("selector")),
            Err(StoreError::Ambiguous(_))
        ));
        assert_eq!(
            fixture
                .store
                .children(root.id())
                .expect("children")
                .iter()
                .map(Node::id)
                .collect::<Vec<_>>(),
            vec![area.id(), sibling.id()]
        );
        assert_eq!(
            fixture
                .store
                .descendants(root.id())
                .expect("descendants")
                .iter()
                .map(|n| n.depth)
                .collect::<Vec<_>>(),
            vec![1, 2, 1, 2]
        );
        assert_eq!(
            fixture
                .store
                .ancestors(leaf.id())
                .expect("ancestors")
                .iter()
                .map(|n| n.depth)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(
            fixture
                .store
                .breadcrumb(leaf.id())
                .expect("breadcrumb")
                .to_string(),
            "Project > Area > Leaf"
        );
        assert_eq!(
            fixture
                .store
                .canonical_path(leaf.id())
                .expect("path")
                .iter()
                .map(Slug::as_str)
                .collect::<Vec<_>>(),
            vec!["project", "area", "leaf"]
        );
    }

    #[test]
    fn multi_node_creation_is_atomic() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let parent = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Parent",
            "parent",
            0,
            1,
        );
        let child = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(parent.id()),
            "Child",
            "child",
            0,
            1,
        );
        let parent_prepared = PreparedNodeMutation {
            node: parent.clone(),
            revision: revision(&parent),
            derived: derived(&parent, 100),
        };
        let mut broken_derived = derived(&child, 200);
        broken_derived.sections[0].node_id = id("01JZ8Q5CWPN8T7KPN5A1V9B6XZ");
        let broken_child = PreparedNodeMutation {
            node: child.clone(),
            revision: revision(&child),
            derived: broken_derived,
        };

        assert!(fixture
            .store
            .create_nodes(&[parent_prepared.clone(), broken_child])
            .is_err());
        assert!(fixture
            .store
            .get(parent.id())
            .expect("parent read")
            .is_none());
        assert!(fixture.store.get(child.id()).expect("child read").is_none());

        let child_prepared = PreparedNodeMutation {
            node: child.clone(),
            revision: revision(&child),
            derived: derived(&child, 300),
        };
        fixture
            .store
            .create_nodes(&[parent_prepared, child_prepared])
            .expect("subtree creation");
        assert!(fixture
            .store
            .get(parent.id())
            .expect("parent read")
            .is_some());
        assert!(fixture.store.get(child.id()).expect("child read").is_some());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn creation_and_updates_are_atomic_and_version_protected() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let child = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Child",
            "child",
            0,
            1,
        );
        let mut broken = derived(&child, 100);
        broken.sections[0].node_id = id("01JZ8Q5CWPN8T7KPN5A1V9B6XZ");
        assert!(fixture
            .store
            .create_node(&child, &revision(&child), &broken)
            .is_err());
        assert!(fixture.store.get(child.id()).expect("read").is_none());

        create(&mut fixture.store, &child, 200);
        let changed = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Renamed",
            "renamed",
            0,
            2,
        );
        let changed_revision = revision(&changed);
        let changed_derived = derived(&changed, 300);
        let mut invalid_update = changed_derived.clone();
        invalid_update.sections[0].node_id = id("01JZ8Q5CWPN8T7KPN5A1V9B6XZ");
        assert!(fixture
            .store
            .update_node(NodeChange {
                node: &changed,
                expected_version: 1,
                revision: &changed_revision,
                derived: &invalid_update,
            })
            .is_err());
        assert_eq!(
            fixture
                .store
                .get(child.id())
                .expect("read")
                .expect("unchanged child")
                .fields()
                .metadata
                .title,
            "Child"
        );
        let change = NodeChange {
            node: &changed,
            expected_version: 1,
            revision: &changed_revision,
            derived: &changed_derived,
        };
        fixture.store.rename_node(change).expect("rename");
        assert_eq!(
            fixture
                .store
                .get(child.id())
                .expect("read")
                .expect("node")
                .id(),
            child.id()
        );
        assert_eq!(
            fixture
                .store
                .breadcrumb(child.id())
                .expect("breadcrumb")
                .to_string(),
            "Project > Renamed"
        );
        assert!(matches!(
            fixture.store.update_node(change),
            Err(StoreError::Conflict { .. })
        ));
        let revisions: u32 = fixture
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM node_versions WHERE node_id=?1",
                [child.id().to_string()],
                |row| row.get(0),
            )
            .expect("revision count");
        assert_eq!(revisions, 2);
        let history = fixture.store.revisions(child.id()).expect("history");
        assert_eq!(
            history
                .iter()
                .map(|revision| revision.version)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(history[0].metadata.title, "Child");
        assert_eq!(history[1].metadata.title, "Renamed");
        assert_eq!(history[1].created_by.as_deref(), Some("test"));
        assert_eq!(
            fixture
                .store
                .revision(child.id(), 1)
                .expect("revision read"),
            Some(history[0].clone())
        );

        assert_eq!(
            fixture
                .store
                .restore_version(
                    child.id(),
                    1,
                    2,
                    3,
                    Some("test-restorer".into()),
                    &SequentialUlidGenerator::new(500),
                )
                .expect("restore"),
            MutationOutcome::Applied
        );
        let restored = fixture
            .store
            .get(child.id())
            .expect("read")
            .expect("restored");
        assert_eq!(restored.fields().version, 3);
        assert_eq!(restored.fields().metadata.title, "Child");
        let fts_title: String = fixture
            .store
            .connection()
            .query_row(
                "SELECT title FROM section_fts WHERE node_id=?1",
                [child.id().to_string()],
                |row| row.get(0),
            )
            .expect("restored FTS row");
        assert_eq!(fts_title, "Child");

        let mut no_op_fields = restored.fields().clone();
        no_op_fields.version = 4;
        no_op_fields.updated_at = 4;
        let no_op = Node::new(no_op_fields, restored.parent_id()).expect("no-op candidate");
        let no_op_revision = revision(&no_op);
        let no_op_derived = derived(&no_op, 600);
        assert_eq!(
            fixture
                .store
                .update_node(NodeChange {
                    node: &no_op,
                    expected_version: 3,
                    revision: &no_op_revision,
                    derived: &no_op_derived,
                })
                .expect("no-op update"),
            MutationOutcome::NoOp
        );
        assert_eq!(
            fixture.store.revisions(child.id()).expect("history").len(),
            3
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn move_reorder_and_removal_enforce_tree_safety() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let area = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Area",
            "area",
            0,
            1,
        );
        let leaf = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(area.id()),
            "Leaf",
            "leaf",
            0,
            1,
        );
        let destination = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XQ",
            Some(root.id()),
            "Destination",
            "destination",
            1,
            1,
        );
        create(&mut fixture.store, &area, 100);
        create(&mut fixture.store, &leaf, 200);
        create(&mut fixture.store, &destination, 250);

        let cyclic = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(leaf.id()),
            "Area",
            "area",
            0,
            2,
        );
        let cyclic_revision = revision(&cyclic);
        let cyclic_derived = derived(&cyclic, 300);
        assert!(matches!(
            fixture.store.move_subtree(NodeChange {
                node: &cyclic,
                expected_version: 1,
                revision: &cyclic_revision,
                derived: &cyclic_derived
            }),
            Err(StoreError::Invariant(_))
        ));

        let moved = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(destination.id()),
            "Area",
            "area",
            0,
            2,
        );
        let moved_revision = revision(&moved);
        let moved_derived = derived(&moved, 350);
        fixture
            .store
            .move_subtree(NodeChange {
                node: &moved,
                expected_version: 1,
                revision: &moved_revision,
                derived: &moved_derived,
            })
            .expect("valid move");
        assert_eq!(
            fixture
                .store
                .breadcrumb(leaf.id())
                .expect("moved breadcrumb")
                .to_string(),
            "Project > Destination > Area > Leaf"
        );

        let reordered = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(destination.id()),
            "Area",
            "area",
            7,
            3,
        );
        let reordered_revision = revision(&reordered);
        let reordered_derived = derived(&reordered, 400);
        fixture
            .store
            .reorder_node(NodeChange {
                node: &reordered,
                expected_version: 2,
                revision: &reordered_revision,
                derived: &reordered_derived,
            })
            .expect("reorder");
        let reopened = SqliteStore::open(&fixture.path).expect("reopen store");
        assert_eq!(
            reopened
                .get(area.id())
                .expect("read")
                .expect("area")
                .fields()
                .sibling_order,
            0
        );
        assert!(fixture.store.remove_subtree(root.id(), 1, false).is_err());
        let impact = fixture
            .store
            .remove_subtree(area.id(), 2, true)
            .expect("dry run");
        assert_eq!(impact.node_count, 2);
        assert!(!impact.deleted);
        assert!(
            fixture
                .store
                .remove_subtree(area.id(), 2, false)
                .expect("delete")
                .deleted
        );
        assert!(fixture.store.get(leaf.id()).expect("read").is_none());
        let orphans: u32 = fixture.store.connection().query_row("SELECT COUNT(*) FROM nodes WHERE parent_id IS NOT NULL AND parent_id NOT IN (SELECT id FROM nodes)", [], |row| row.get(0)).expect("orphan count");
        assert_eq!(orphans, 0);
    }

    #[test]
    fn reorder_inserts_at_exact_index_preserves_references_and_revisions_siblings() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let specs = [
            ("01JZ8Q5CWPN8T7KPN5A1V9B6XN", "Alpha", "alpha"),
            ("01JZ8Q5CWPN8T7KPN5A1V9B6XP", "Bravo", "bravo"),
            ("01JZ8Q5CWPN8T7KPN5A1V9B6XQ", "Charlie", "charlie"),
            ("01JZ8Q5CWPN8T7KPN5A1V9B6XR", "Delta", "delta"),
        ];
        for (order, (raw_id, title, slug)) in specs.iter().enumerate() {
            let child = node(
                raw_id,
                Some(root.id()),
                title,
                slug,
                u32::try_from(order).expect("order"),
                1,
            );
            create(
                &mut fixture.store,
                &child,
                100 + u64::try_from(order).expect("seed"),
            );
        }
        let alpha = id(specs[0].0);
        let delta = id(specs[3].0);
        fixture
            .store
            .connection()
            .execute(
                "INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin) VALUES (?1,?2,?2,'depends_on','explicit')",
                params![alpha.to_string(), delta.to_string()],
            )
            .expect("explicit reference");

        let current = fixture.store.get(alpha).expect("read").expect("alpha");
        let prepared = prepared_reorder(&current, 2, 500);
        assert!(matches!(
            fixture
                .store
                .reorder_node(prepared.change(current.fields().version)),
            Ok(MutationOutcome::Applied)
        ));
        let children = fixture.store.children(root.id()).expect("children");
        assert_eq!(
            children
                .iter()
                .map(|child| child.fields().metadata.title.as_str())
                .collect::<Vec<_>>(),
            ["Bravo", "Charlie", "Alpha", "Delta"]
        );

        // Exercise the opposite ULID tie-break direction as well: the largest
        // ID moves onto an index currently occupied by a smaller ID.
        let current = fixture.store.get(delta).expect("read").expect("delta");
        let prepared = prepared_reorder(&current, 1, 600);
        assert!(matches!(
            fixture
                .store
                .reorder_node(prepared.change(current.fields().version)),
            Ok(MutationOutcome::Applied)
        ));
        let children = fixture.store.children(root.id()).expect("children");
        assert_eq!(
            children
                .iter()
                .map(|child| child.fields().metadata.title.as_str())
                .collect::<Vec<_>>(),
            ["Bravo", "Delta", "Charlie", "Alpha"]
        );
        assert!(children.iter().enumerate().all(|(index, child)| {
            child.fields().sibling_order == u32::try_from(index).expect("order")
        }));
        assert_eq!(
            fixture
                .store
                .outgoing_references(alpha)
                .expect("references")
                .len(),
            1
        );
        for child in children {
            let latest = fixture
                .store
                .revisions(child.id())
                .expect("history")
                .pop()
                .expect("latest revision");
            assert_eq!(latest.version, child.fields().version);
            assert_eq!(latest.sibling_order, child.fields().sibling_order);
            assert_eq!(latest.revision_hash, child.fields().revision_hash);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn focused_and_heterogeneous_batches_are_atomic_and_dry_runnable() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let left = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B701",
            Some(root.id()),
            "Left",
            "left",
            0,
            1,
        );
        let right = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B702",
            Some(root.id()),
            "Right",
            "right",
            1,
            1,
        );
        let destination = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B703",
            Some(root.id()),
            "Destination",
            "destination",
            2,
            1,
        );
        let trash = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B704",
            Some(root.id()),
            "Trash",
            "trash",
            3,
            1,
        );
        for (offset, item) in [&left, &right, &destination, &trash]
            .into_iter()
            .enumerate()
        {
            create(
                &mut fixture.store,
                item,
                100 + u64::try_from(offset).expect("offset"),
            );
        }
        let moved = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B701",
            Some(destination.id()),
            "Left",
            "left",
            0,
            2,
        );
        let move_item = AtomicTreeMove {
            prepared: PreparedNodeMutation {
                node: moved.clone(),
                revision: revision(&moved),
                derived: derived(&moved, 200),
            },
            expected_version: 1,
        };
        let removal = AtomicTreeRemoval {
            node_id: trash.id(),
            expected_version: 1,
        };
        let planned = fixture
            .store
            .apply_atomic_tree_batch(std::slice::from_ref(&move_item), &[removal], true)
            .expect("focused dry run");
        assert_eq!(planned.status, "planned");
        assert_eq!(
            fixture
                .store
                .get(left.id())
                .expect("left")
                .expect("left")
                .parent_id(),
            Some(root.id())
        );
        assert!(fixture.store.get(trash.id()).expect("trash").is_some());
        fixture
            .store
            .apply_atomic_tree_batch(&[move_item], &[removal], false)
            .expect("focused apply");
        assert_eq!(
            fixture
                .store
                .get(left.id())
                .expect("left")
                .expect("left")
                .parent_id(),
            Some(destination.id())
        );
        assert!(fixture.store.get(trash.id()).expect("trash").is_none());

        let renamed = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B702",
            Some(root.id()),
            "Renamed",
            "renamed",
            0,
            2,
        );
        let created = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B705",
            Some(root.id()),
            "Created",
            "created",
            4,
            1,
        );
        let operations = vec![
            PreparedBatchOperation::Replace {
                prepared: PreparedNodeMutation {
                    node: renamed.clone(),
                    revision: revision(&renamed),
                    derived: derived(&renamed, 300),
                },
                expected_version: 1,
            },
            PreparedBatchOperation::Create(PreparedNodeMutation {
                node: created.clone(),
                revision: revision(&created),
                derived: derived(&created, 400),
            }),
        ];
        assert_eq!(
            fixture
                .store
                .apply_mutation_batch(&operations, true)
                .expect("generic dry run")
                .status,
            "planned"
        );
        assert!(fixture.store.get(created.id()).expect("created").is_none());
        fixture
            .store
            .apply_mutation_batch(&operations, false)
            .expect("generic apply");
        assert_eq!(
            fixture
                .store
                .get(right.id())
                .expect("right")
                .expect("right")
                .fields()
                .metadata
                .title,
            "Renamed"
        );
        assert!(fixture.store.get(created.id()).expect("created").is_some());

        let rolled_back = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B702",
            Some(root.id()),
            "Rolled Back",
            "rolled-back",
            0,
            3,
        );
        let broken = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B706",
            Some(root.id()),
            "Broken",
            "broken",
            5,
            1,
        );
        let mut broken_derived = derived(&broken, 500);
        broken_derived.sections[0].node_id = id("01JZ8Q5CWPN8T7KPN5A1V9B799");
        let failing = vec![
            PreparedBatchOperation::Replace {
                prepared: PreparedNodeMutation {
                    node: rolled_back.clone(),
                    revision: revision(&rolled_back),
                    derived: derived(&rolled_back, 600),
                },
                expected_version: 2,
            },
            PreparedBatchOperation::Create(PreparedNodeMutation {
                node: broken.clone(),
                revision: revision(&broken),
                derived: broken_derived,
            }),
        ];
        assert!(fixture.store.apply_mutation_batch(&failing, false).is_err());
        assert_eq!(
            fixture
                .store
                .get(right.id())
                .expect("right")
                .expect("right")
                .fields()
                .metadata
                .title,
            "Renamed"
        );
        assert!(fixture.store.get(broken.id()).expect("broken").is_none());
    }

    #[test]
    fn workspace_revision_advances_and_is_visible_to_a_second_open_handle() {
        let mut fixture = fixture();
        let other = SqliteStore::open(&fixture.path).expect("second handle on the same file");

        let baseline = fixture.store.workspace_revision().expect("revision");
        assert_eq!(other.workspace_revision().expect("revision"), baseline);

        let root = fixture.store.root().expect("root");
        let cursor_scope =
            CursorScope::new("children", Some(root.id()), "parent=project").expect("cursor scope");
        let cursor = PageCursor::issue(
            baseline,
            cursor_scope.clone(),
            PagePosition::Sibling {
                sibling_order: 0,
                node_id: root.id(),
            },
        )
        .expect("cursor");
        let child = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Child",
            "child",
            0,
            1,
        );
        create(&mut fixture.store, &child, 200);

        assert!(fixture.store.workspace_revision().expect("revision") > baseline);
        assert_eq!(
            cursor
                .resume(
                    &cursor_scope,
                    fixture.store.workspace_revision().expect("revision")
                )
                .expect_err("create must stale cursor")
                .code(),
            PaginationErrorCode::StaleCursor
        );
        assert_eq!(
            other.workspace_revision().expect("revision"),
            fixture.store.workspace_revision().expect("revision"),
            "a second handle on the same file must observe the same advanced revision"
        );

        let after_create = other.workspace_revision().expect("revision");
        let renamed = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Renamed",
            "renamed",
            0,
            2,
        );
        fixture
            .store
            .rename_node(NodeChange {
                node: &renamed,
                expected_version: 1,
                revision: &revision(&renamed),
                derived: &derived(&renamed, 300),
            })
            .expect("rename");
        assert!(other.workspace_revision().expect("revision") > after_create);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolves_reads_and_mutates_typed_references() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let source = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Source",
            "source",
            0,
            1,
        );
        let target = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(root.id()),
            "Target",
            "target",
            1,
            1,
        );
        let duplicate = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XQ",
            Some(root.id()),
            "Target",
            "other-target",
            2,
            1,
        );
        create(&mut fixture.store, &source, 100);
        create(&mut fixture.store, &target, 200);
        create(&mut fixture.store, &duplicate, 300);

        assert!(
            matches!(fixture.store.resolve_reference(&target.id().to_string()).expect("ID resolution"),
            ReferenceResolution::Resolved { node_id, .. } if node_id==target.id())
        );
        assert!(
            matches!(fixture.store.resolve_reference("project/target#target").expect("path resolution"),
            ReferenceResolution::Resolved { node_id, anchor:Some(_)} if node_id==target.id())
        );
        assert!(
            matches!(fixture.store.resolve_reference("Target").expect("title resolution"),
            ReferenceResolution::Ambiguous { candidates } if candidates.len()==2)
        );
        assert_eq!(
            fixture
                .store
                .resolve_reference("Missing")
                .expect("unresolved"),
            ReferenceResolution::Unresolved
        );

        let changed = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Source",
            "source",
            0,
            2,
        );
        let changed_revision = revision(&changed);
        let changed_derived = derived(&changed, 400);
        let explicit = Reference {
            source_node_id: source.id(),
            source_section_id: None,
            reference_type: ReferenceType::from_str("depends_on").expect("type"),
            target: ReferenceTarget::Resolved {
                node_id: target.id(),
                target_ref: Some("Target".into()),
                anchor: None,
            },
            origin: ReferenceOrigin::Explicit,
            metadata: std::collections::BTreeMap::new(),
        };
        let unresolved_explicit = Reference {
            source_node_id: source.id(),
            source_section_id: None,
            reference_type: ReferenceType::from_str("related_to").expect("type"),
            target: ReferenceTarget::Unresolved {
                target_ref: "Missing target".into(),
            },
            origin: ReferenceOrigin::Explicit,
            metadata: std::collections::BTreeMap::new(),
        };
        assert!(matches!(
            fixture.store.set_explicit_references(
                NodeChange {
                    node: &changed,
                    expected_version: 1,
                    revision: &changed_revision,
                    derived: &changed_derived
                },
                &[explicit.clone(), explicit.clone()]
            ),
            Err(StoreError::Invariant(_))
        ));
        fixture
            .store
            .set_explicit_references(
                NodeChange {
                    node: &changed,
                    expected_version: 1,
                    revision: &changed_revision,
                    derived: &changed_derived,
                },
                &[explicit.clone(), unresolved_explicit.clone()],
            )
            .expect("add explicit reference");
        assert!(fixture
            .store
            .outgoing_references(source.id())
            .expect("outgoing")
            .contains(&explicit));
        assert!(fixture
            .store
            .outgoing_references(source.id())
            .expect("outgoing")
            .contains(&unresolved_explicit));
        assert!(fixture
            .store
            .backlinks(target.id())
            .expect("backlinks")
            .contains(&explicit));

        let removed = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Source",
            "source",
            0,
            3,
        );
        let removed_revision = revision(&removed);
        let removed_derived = derived(&removed, 500);
        fixture
            .store
            .set_explicit_references(
                NodeChange {
                    node: &removed,
                    expected_version: 2,
                    revision: &removed_revision,
                    derived: &removed_derived,
                },
                &[],
            )
            .expect("remove explicit reference");
        assert!(fixture
            .store
            .backlinks(target.id())
            .expect("backlinks")
            .is_empty());
    }

    #[test]
    fn integrity_diagnostics_and_rebuild_are_deterministic() {
        let mut fixture = fixture();
        let root = fixture.store.root().expect("root");
        let first = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XN",
            Some(root.id()),
            "Duplicate",
            "first",
            0,
            1,
        );
        let second = node(
            "01JZ8Q5CWPN8T7KPN5A1V9B6XP",
            Some(root.id()),
            "Duplicate",
            "second",
            1,
            1,
        );
        create(&mut fixture.store, &first, 100);
        create(&mut fixture.store, &second, 200);
        fixture.store.connection().execute("INSERT INTO \"references\" (source_node_id,target_ref,reference_type,origin) VALUES (?1,'Missing','references','agent')",[first.id().to_string()]).expect("unresolved fixture");
        let duplicates = fixture.store.duplicate_candidates().expect("duplicates");
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].node_ids.len(), 2);
        assert_eq!(duplicates[0].breadcrumbs.len(), 2);
        let unresolved = fixture.store.unresolved_references().expect("unresolved");
        assert!(unresolved
            .iter()
            .any(|item| item.reference.origin == ReferenceOrigin::Agent));
        assert!(unresolved
            .iter()
            .all(|item| !item.source_breadcrumb.segments().is_empty()));

        let before: u32 = fixture
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM section_fts WHERE section_fts MATCH 'duplicate'",
                [],
                |row| row.get(0),
            )
            .expect("search before");

        fixture
            .store
            .connection()
            .execute(
                "DELETE FROM section_fts WHERE node_id=?1",
                [first.id().to_string()],
            )
            .expect("corrupt FTS");
        fixture
            .store
            .connection()
            .execute(
                "UPDATE nodes SET sibling_order=9 WHERE id=?1",
                [second.id().to_string()],
            )
            .expect("corrupt order");
        let report = fixture
            .store
            .validate_integrity()
            .expect("integrity report");
        let codes: Vec<_> = report.findings.iter().map(|finding| finding.code).collect();
        assert!(codes.contains(&"fts_consistency"));
        assert!(codes.contains(&"sibling_order"));
        assert!(codes.contains(&"content_hash"));

        fixture
            .store
            .connection()
            .execute("DELETE FROM section_fts", [])
            .expect("delete FTS");
        fixture
            .store
            .connection()
            .execute("DELETE FROM \"references\"", [])
            .expect("delete refs");
        fixture
            .store
            .connection()
            .execute("DELETE FROM sections", [])
            .expect("delete sections");
        fixture
            .store
            .rebuild_derived(&SequentialUlidGenerator::new(1000))
            .expect("rebuild");
        let after: u32 = fixture
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM section_fts WHERE section_fts MATCH 'duplicate'",
                [],
                |row| row.get(0),
            )
            .expect("search after");
        assert_eq!(after, before);
    }
}
