//! `SQLite` persistence adapter for `MDTree`.

mod connection;
mod context;
mod maintenance;
mod migrations;
mod mutation_assembly;
mod projection;
mod search;
mod snapshot;
mod store;
mod workspace;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use connection::{open_connection, open_memory_connection};
pub use maintenance::{
    backup_workspace, check_workspace, doctor_workspace, restore_workspace, CheckReport,
    CheckStatus, DoctorFinding, DoctorReport, MaintenanceError,
};
pub use migrations::{migrate, MigrationError, LATEST_SCHEMA_VERSION, WORKSPACE_FORMAT_VERSION};
pub use mutation_assembly::{prepare_node_mutation, NodeMutationDraft, PreparedNodeMutation};
pub use projection::{IntegrityPage, PageReadError};
pub use snapshot::{
    export_markdown_node, export_markdown_snapshot, export_snapshot, export_snapshot_json,
    import_markdown_snapshot_new, import_snapshot_new, plan_json_import, plan_markdown_import,
    ImportPlan, SnapshotError,
};
pub use store::{
    AtomicTreeBatchResult, AtomicTreeMove, AtomicTreeRemoval, DuplicateCandidate,
    HistoryPruneReport, IntegrityFinding, IntegrityReport, MutationBatchResult, MutationOutcome,
    NodeChange, NodeDepth, PreparedBatchOperation, ReferenceResolution, RemovalImpact, SqliteStore,
    StoreError, UnresolvedDiagnostic,
};
pub use workspace::{
    create_workspace, open_workspace, workspace_status, WorkspaceError, WorkspaceStatus,
};
