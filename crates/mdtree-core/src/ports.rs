//! Infrastructure-independent persistence and query ports.
//!
//! Every fallible method may return any [`crate::ApplicationError`] supplied
//! by its adapter, so repeating an identical Errors section adds no detail.

#![allow(clippy::missing_errors_doc)]

use crate::{
    ApplicationError, Node, NodeId, NodeRevision, NodeSelector, Reference, SearchMatch,
    SearchRequest, Section,
};

/// Result returned across repository boundaries.
pub type RepositoryResult<T> = Result<T, ApplicationError>;

/// Reads workspace-level canonical state.
pub trait WorkspaceRepository {
    /// Returns the workspace root node.
    fn root(&self) -> RepositoryResult<Node>;

    /// Returns the persisted workspace format version.
    fn workspace_format_version(&self) -> RepositoryResult<u32>;
}

/// Reads and mutates individual canonical nodes.
pub trait NodeRepository {
    /// Loads a node by stable identity.
    fn get_node(&self, id: NodeId) -> RepositoryResult<Option<Node>>;

    /// Resolves a human-facing selector, reporting ambiguity through the error model.
    fn resolve_node(&self, selector: &NodeSelector) -> RepositoryResult<Option<Node>>;

    /// Inserts a new canonical node.
    fn insert_node(&mut self, node: &Node) -> RepositoryResult<()>;

    /// Replaces canonical state for an existing node.
    fn update_node(&mut self, node: &Node) -> RepositoryResult<()>;

    /// Removes one canonical node after application-level policy checks.
    fn delete_node(&mut self, id: NodeId) -> RepositoryResult<()>;
}

/// Traverses canonical parent-child structure.
pub trait TreeRepository {
    /// Returns the canonical parent, if the node is not the root.
    fn parent(&self, id: NodeId) -> RepositoryResult<Option<Node>>;

    /// Returns children in deterministic sibling order.
    fn children(&self, id: NodeId) -> RepositoryResult<Vec<Node>>;

    /// Returns ancestors ordered from root to immediate parent.
    fn ancestors(&self, id: NodeId) -> RepositoryResult<Vec<Node>>;

    /// Returns descendants in deterministic depth-first tree order.
    fn descendants(&self, id: NodeId) -> RepositoryResult<Vec<Node>>;
}

/// Stores derived Markdown sections.
pub trait SectionRepository {
    /// Returns sections in document order.
    fn sections(&self, node_id: NodeId) -> RepositoryResult<Vec<Section>>;

    /// Atomically replaces all derived sections for a node within the active transaction.
    fn replace_sections(&mut self, node_id: NodeId, sections: &[Section]) -> RepositoryResult<()>;
}

/// Stores typed secondary relationships.
pub trait ReferenceRepository {
    /// Returns references originating at a node.
    fn outgoing_references(&self, node_id: NodeId) -> RepositoryResult<Vec<Reference>>;

    /// Returns references resolved to a node.
    fn backlinks(&self, node_id: NodeId) -> RepositoryResult<Vec<Reference>>;

    /// Atomically replaces the references owned by one source node.
    fn replace_references(
        &mut self,
        source_node_id: NodeId,
        references: &[Reference],
    ) -> RepositoryResult<()>;
}

/// Stores immutable node revision history.
pub trait RevisionRepository {
    /// Returns revisions ordered by ascending version.
    fn revisions(&self, node_id: NodeId) -> RepositoryResult<Vec<NodeRevision>>;

    /// Returns one exact historical version.
    fn revision(&self, node_id: NodeId, version: u64) -> RepositoryResult<Option<NodeRevision>>;

    /// Appends one immutable revision.
    fn append_revision(&mut self, revision: &NodeRevision) -> RepositoryResult<()>;
}

/// Queries the derived search index without defining ranking policy in adapters.
pub trait SearchRepository {
    /// Finds section-oriented matches for a normalized request.
    fn search(&self, request: &SearchRequest) -> RepositoryResult<Vec<SearchMatch>>;
}

/// Controls the atomic boundary shared by all mutating repository operations.
pub trait TransactionRepository {
    /// Starts a write transaction.
    fn begin_transaction(&mut self) -> RepositoryResult<()>;

    /// Commits the active transaction.
    fn commit_transaction(&mut self) -> RepositoryResult<()>;

    /// Rolls back the active transaction.
    fn rollback_transaction(&mut self) -> RepositoryResult<()>;
}

#[cfg(test)]
mod tests {
    use super::{
        NodeRepository, ReferenceRepository, RepositoryResult, RevisionRepository,
        SearchRepository, SectionRepository, TransactionRepository, TreeRepository,
        WorkspaceRepository,
    };
    use crate::{
        ApplicationError, Node, NodeId, NodeRevision, NodeSelector, Reference, SearchMatch,
        SearchRequest, Section,
    };

    #[derive(Default)]
    struct InMemoryStub {
        transaction_active: bool,
    }

    fn unavailable<T>() -> RepositoryResult<T> {
        Err(ApplicationError::Unsupported("empty test stub".into()))
    }

    impl WorkspaceRepository for InMemoryStub {
        fn root(&self) -> RepositoryResult<Node> {
            unavailable()
        }

        fn workspace_format_version(&self) -> RepositoryResult<u32> {
            Ok(1)
        }
    }

    impl NodeRepository for InMemoryStub {
        fn get_node(&self, _id: NodeId) -> RepositoryResult<Option<Node>> {
            Ok(None)
        }

        fn resolve_node(&self, _selector: &NodeSelector) -> RepositoryResult<Option<Node>> {
            Ok(None)
        }

        fn insert_node(&mut self, _node: &Node) -> RepositoryResult<()> {
            Ok(())
        }

        fn update_node(&mut self, _node: &Node) -> RepositoryResult<()> {
            Ok(())
        }

        fn delete_node(&mut self, _id: NodeId) -> RepositoryResult<()> {
            Ok(())
        }
    }

    impl TreeRepository for InMemoryStub {
        fn parent(&self, _id: NodeId) -> RepositoryResult<Option<Node>> {
            Ok(None)
        }

        fn children(&self, _id: NodeId) -> RepositoryResult<Vec<Node>> {
            Ok(Vec::new())
        }

        fn ancestors(&self, _id: NodeId) -> RepositoryResult<Vec<Node>> {
            Ok(Vec::new())
        }

        fn descendants(&self, _id: NodeId) -> RepositoryResult<Vec<Node>> {
            Ok(Vec::new())
        }
    }

    impl SectionRepository for InMemoryStub {
        fn sections(&self, _node_id: NodeId) -> RepositoryResult<Vec<Section>> {
            Ok(Vec::new())
        }

        fn replace_sections(
            &mut self,
            _node_id: NodeId,
            _sections: &[Section],
        ) -> RepositoryResult<()> {
            Ok(())
        }
    }

    impl ReferenceRepository for InMemoryStub {
        fn outgoing_references(&self, _node_id: NodeId) -> RepositoryResult<Vec<Reference>> {
            Ok(Vec::new())
        }

        fn backlinks(&self, _node_id: NodeId) -> RepositoryResult<Vec<Reference>> {
            Ok(Vec::new())
        }

        fn replace_references(
            &mut self,
            _source_node_id: NodeId,
            _references: &[Reference],
        ) -> RepositoryResult<()> {
            Ok(())
        }
    }

    impl RevisionRepository for InMemoryStub {
        fn revisions(&self, _node_id: NodeId) -> RepositoryResult<Vec<NodeRevision>> {
            Ok(Vec::new())
        }

        fn revision(
            &self,
            _node_id: NodeId,
            _version: u64,
        ) -> RepositoryResult<Option<NodeRevision>> {
            Ok(None)
        }

        fn append_revision(&mut self, _revision: &NodeRevision) -> RepositoryResult<()> {
            Ok(())
        }
    }

    impl SearchRepository for InMemoryStub {
        fn search(&self, _request: &SearchRequest) -> RepositoryResult<Vec<SearchMatch>> {
            Ok(Vec::new())
        }
    }

    impl TransactionRepository for InMemoryStub {
        fn begin_transaction(&mut self) -> RepositoryResult<()> {
            self.transaction_active = true;
            Ok(())
        }

        fn commit_transaction(&mut self) -> RepositoryResult<()> {
            self.transaction_active = false;
            Ok(())
        }

        fn rollback_transaction(&mut self) -> RepositoryResult<()> {
            self.transaction_active = false;
            Ok(())
        }
    }

    fn assert_every_port<T>()
    where
        T: WorkspaceRepository
            + NodeRepository
            + TreeRepository
            + SectionRepository
            + ReferenceRepository
            + RevisionRepository
            + SearchRepository
            + TransactionRepository,
    {
    }

    #[test]
    fn infrastructure_free_stub_implements_every_port() {
        assert_every_port::<InMemoryStub>();

        let mut stub = InMemoryStub::default();
        assert_eq!(stub.workspace_format_version(), Ok(1));
        stub.begin_transaction().expect("begin transaction");
        assert!(stub.transaction_active);
        stub.rollback_transaction().expect("rollback transaction");
        assert!(!stub.transaction_active);
    }
}
