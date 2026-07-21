//! Domain model and application services for `MDTree`.

mod benchmark_fixture;
mod context;
mod error;
mod example_fixtures;
mod fixture;
mod hashing;
mod identity;
mod metadata;
mod node;
mod pagination;
mod ports;
mod projection;
mod providers;
mod records;
mod revision_diff;
mod scale_fixture;
mod search;
mod slug_generation;
mod snapshot;
mod subtree_diff;

pub use benchmark_fixture::generate_benchmark_snapshot;
pub use context::{
    ContextSummary, ConventionSummary, InspectionItem, ReadContext, SubtreeInspection, WriteContext,
};
pub use error::{ApplicationError, DomainError, ErrorCode, ErrorReport};
pub use example_fixtures::developer_workspace_snapshot;
pub use fixture::northstar_platform_snapshot;
pub use hashing::{hash_content, hash_revision, RevisionHashInput};
pub use identity::{Breadcrumb, NodeId, NodeSelector, Slug};
pub use metadata::{NodeMetadata, NodeType};
pub use node::{Node, NodeFields, NodeHash};
pub use pagination::{
    CursorScope, Page, PageCursor, PageLimit, PagePosition, PaginationError, PaginationErrorCode,
    DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT, MIN_PAGE_LIMIT,
};
pub use ports::{
    NodeRepository, ReferenceRepository, RepositoryResult, RevisionRepository, SearchRepository,
    SectionRepository, TransactionRepository, TreeRepository, WorkspaceRepository,
};
pub use projection::{
    project_canonical_nodes, AdjacentSiblings, AncestorContainment, BatchChildrenLookupItem,
    BatchChildrenRequest, BatchLookupError, BatchNodeLookupItem, CloneSubtreeRequest,
    CloneSubtreeResult, IndexedChild, NodeChildCount, NodeDepthProjection, NodeProjection,
    NodeSummaryProjection, PathBetween, RevisionSummary, StructuralPredicate, TraversalOrder,
    TreeDistance, TreeStatistics, MAX_BATCH_CHILDREN, MAX_BATCH_ITEMS, MAX_BATCH_PARENTS,
};
pub use providers::{
    Clock, FixedClock, SequentialUlidGenerator, SystemClock, SystemUlidGenerator, UlidGenerator,
};
pub use records::{
    NodeRevision, Reference, ReferenceOrigin, ReferenceTarget, ReferenceType, Section,
};
pub use revision_diff::{diff_revisions, RevisionDiff, RevisionField};
pub use scale_fixture::{
    generate_large_tree_fixture, LargeTreeFixture, LargeTreeFixtureSpec,
    DEFAULT_RESPONSE_BOUNDARY_BYTES,
};
pub use search::{
    normalize_fts_query, DestinationCandidate, LocateAction, LocateResult, LocateStatus,
    SearchFilters, SearchMatch, SearchRequest, SearchScope,
};
pub use slug_generation::{generate_slug, slug_for_rename, RenameSlugPolicy};
pub use snapshot::{
    validate_snapshot, RevisionPolicy, Snapshot, SnapshotNode, SnapshotValidationError,
    SnapshotValidationReport, SnapshotWorkspace, SNAPSHOT_FORMAT_VERSION,
};
pub use subtree_diff::{diff_subtrees, SubtreeChange, SubtreeDiffItem};
