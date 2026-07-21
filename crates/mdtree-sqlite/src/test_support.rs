//! Feature-gated read-work observability for integration and adapter tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mdtree_core::NodeId;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use thiserror::Error;

use crate::{SqliteStore, WorkspaceError};

const PROGRESS_INTERVAL: i32 = 100;

#[derive(Debug, Default)]
struct ObserverState {
    select_statements: AtomicU64,
    estimated_vm_steps: AtomicU64,
    write_authorizations: AtomicU64,
    table_reads: Mutex<BTreeMap<String, u64>>,
}

/// Cloneable observer installed on one test-owned `SQLite` connection.
///
/// `SQLite` authorizer callbacks provide deterministic statement/table evidence,
/// while the progress callback supplies timing-independent virtual-machine work.
#[derive(Clone, Debug, Default)]
pub struct ReadQueryObserver {
    state: Arc<ObserverState>,
}

impl ReadQueryObserver {
    fn install(&self, store: &SqliteStore) -> rusqlite::Result<()> {
        let authorizer_state = Arc::clone(&self.state);
        store
            .connection()
            .authorizer(Some(move |context: AuthContext<'_>| {
                match context.action {
                    AuthAction::Select => {
                        authorizer_state
                            .select_statements
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    AuthAction::Read { table_name, .. } => {
                        let mut reads = authorizer_state
                            .table_reads
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *reads.entry(table_name.into()).or_default() += 1;
                    }
                    AuthAction::Insert { .. }
                    | AuthAction::Update { .. }
                    | AuthAction::Delete { .. } => {
                        authorizer_state
                            .write_authorizations
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                Authorization::Allow
            }))?;
        let progress_state = Arc::clone(&self.state);
        store.connection().progress_handler(
            PROGRESS_INTERVAL,
            Some(move || {
                progress_state
                    .estimated_vm_steps
                    .fetch_add(PROGRESS_INTERVAL as u64, Ordering::Relaxed);
                false
            }),
        )?;
        Ok(())
    }

    /// Clears accumulated evidence immediately before the read under test.
    pub fn reset(&self) {
        self.state.select_statements.store(0, Ordering::Relaxed);
        self.state.estimated_vm_steps.store(0, Ordering::Relaxed);
        self.state.write_authorizations.store(0, Ordering::Relaxed);
        self.state
            .table_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Returns a stable copy of all evidence accumulated since the last reset.
    #[must_use]
    pub fn observation(&self) -> ReadQueryObservation {
        ReadQueryObservation {
            select_statements: self.state.select_statements.load(Ordering::Relaxed),
            estimated_vm_steps: self.state.estimated_vm_steps.load(Ordering::Relaxed),
            write_authorizations: self.state.write_authorizations.load(Ordering::Relaxed),
            table_reads: self
                .state
                .table_reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

/// Timing-independent evidence for one observed storage read.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadQueryObservation {
    /// SELECT authorizations seen while `SQLite` prepared statements.
    pub select_statements: u64,
    /// Completed `SQLite` virtual-machine steps, rounded down to the observer interval.
    pub estimated_vm_steps: u64,
    /// INSERT, UPDATE, and DELETE authorizations seen during a purported read.
    pub write_authorizations: u64,
    /// Column-read authorizations grouped by canonical table name.
    pub table_reads: BTreeMap<String, u64>,
}

impl ReadQueryObservation {
    /// Number of read authorizations involving one table.
    #[must_use]
    pub fn table_read_count(&self, table: &str) -> u64 {
        self.table_reads.get(table).copied().unwrap_or(0)
    }

    /// Whether a complete-export signature loaded both history and references.
    #[must_use]
    pub fn has_complete_snapshot_signature(&self) -> bool {
        self.table_read_count("nodes") > 0
            && self.table_read_count("node_versions") > 0
            && self.table_read_count("references") > 0
    }
}

/// Evidence from the repeated linear lookup pattern used by legacy projections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinearLookupObservation {
    /// Requested identities.
    pub requests: usize,
    /// Candidate identities searched for every request.
    pub candidates: usize,
    /// Equality comparisons actually performed.
    pub comparisons: u64,
    /// Requests that found a candidate.
    pub matched: usize,
}

impl LinearLookupObservation {
    /// Whether comparison work exceeds one linear pass over both inputs.
    #[must_use]
    pub fn is_repeated_linear_mapping(&self) -> bool {
        self.comparisons
            > u64::try_from(self.requests.saturating_add(self.candidates)).unwrap_or(u64::MAX)
    }
}

/// Runs and counts the legacy `requested × candidates.iter().find(...)` pattern.
///
/// This utility makes projection-algorithm work measurable without wall-clock timing.
#[must_use]
pub fn observe_linear_lookup(
    requested: &[NodeId],
    candidates: &[NodeId],
) -> LinearLookupObservation {
    let mut comparisons = 0_u64;
    let mut matched = 0;
    for requested_id in requested {
        if candidates.iter().any(|candidate| {
            comparisons = comparisons.saturating_add(1);
            candidate == requested_id
        }) {
            matched += 1;
        }
    }
    LinearLookupObservation {
        requests: requested.len(),
        candidates: candidates.len(),
        comparisons,
        matched,
    }
}

/// Failure while opening or instrumenting an observed test store.
#[derive(Debug, Error)]
pub enum ObservedStoreError {
    /// Workspace validation/opening failed.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// `SQLite` rejected an observation hook.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Opens a normal validated store and attaches test-only read observability.
///
/// # Errors
///
/// Returns [`ObservedStoreError`] when the workspace cannot be opened or an
/// observation callback cannot be installed.
pub fn open_observed_store(
    path: &Path,
) -> Result<(SqliteStore, ReadQueryObserver), ObservedStoreError> {
    let store = SqliteStore::open(path)?;
    let observer = ReadQueryObserver::default();
    observer.install(&store)?;
    observer.reset();
    Ok((store, observer))
}

#[cfg(test)]
mod tests {
    use mdtree_core::{generate_large_tree_fixture, LargeTreeFixtureSpec, PageLimit};
    use tempfile::tempdir;

    use super::{observe_linear_lookup, open_observed_store};
    use crate::{export_snapshot, import_snapshot_new};

    fn fixture_spec() -> LargeTreeFixtureSpec {
        LargeTreeFixtureSpec {
            wide_children: 24,
            deep_descendants: 16,
            history_revisions: 12,
            relations: 32,
            response_boundary_bytes: 4096,
        }
    }

    #[test]
    fn observer_distinguishes_targeted_reads_from_complete_snapshot_export() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("observed.mdtree");
        let fixture = generate_large_tree_fixture(fixture_spec(), 81);
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let (store, observer) = open_observed_store(&path).expect("observed store");

        store.get(fixture.history_node_id).expect("targeted node");
        let targeted = observer.observation();
        assert!(!targeted.has_complete_snapshot_signature());
        assert_eq!(targeted.table_read_count("node_versions"), 0);
        assert_eq!(targeted.table_read_count("references"), 0);

        observer.reset();
        export_snapshot(&store).expect("complete export");
        let complete = observer.observation();
        assert!(complete.has_complete_snapshot_signature());
        assert!(complete.select_statements > targeted.select_statements);
        assert!(complete.estimated_vm_steps > targeted.estimated_vm_steps);
        assert_eq!(complete.write_authorizations, 0);
    }

    #[test]
    fn observer_proves_inspection_uses_bounded_lookahead_and_batched_counts() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("inspection.mdtree");
        let fixture = generate_large_tree_fixture(fixture_spec(), 82);
        import_snapshot_new(&path, &fixture.snapshot).expect("scale workspace");
        let (store, observer) = open_observed_store(&path).expect("observed store");

        store
            .subtree(fixture.wide_parent_id)
            .expect("one subtree scan");
        let complete_scan = observer.observation();

        observer.reset();
        let single = store
            .inspect_subtree_page(
                "inspect",
                fixture.wide_parent_id,
                1,
                PageLimit::new(1).expect("limit"),
                None,
            )
            .expect("single inspection item");
        let single_observation = observer.observation();

        observer.reset();
        let inspection = store
            .inspect_subtree_page(
                "inspect",
                fixture.wide_parent_id,
                1,
                PageLimit::new(8).expect("limit"),
                None,
            )
            .expect("bounded inspection");
        let inspection_observation = observer.observation();

        assert_eq!(single.items.len(), 1);
        assert_eq!(inspection.items.len(), 8);
        assert_eq!(inspection.items[0].child_count, 24);
        assert!(inspection.items[1..]
            .iter()
            .all(|item| item.child_count == 0));
        assert!(inspection.truncated);
        assert!(inspection.next_cursor.is_some());
        assert!(inspection_observation.estimated_vm_steps < complete_scan.estimated_vm_steps);
        assert_eq!(
            inspection_observation.select_statements, single_observation.select_statements,
            "child counts must not add one SELECT per returned item"
        );
        assert_eq!(inspection_observation.write_authorizations, 0);
        assert_eq!(inspection_observation.table_read_count("node_versions"), 0);
        assert_eq!(inspection_observation.table_read_count("references"), 0);
        assert!(!inspection_observation.has_complete_snapshot_signature());

        let depth_limited = store
            .inspect_subtree_page(
                "inspect",
                fixture.deep_parent_id,
                2,
                PageLimit::new(10).expect("limit"),
                None,
            )
            .expect("depth-limited inspection");
        assert_eq!(
            depth_limited
                .items
                .iter()
                .map(|item| (item.depth, item.child_count))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 1), (2, 1)]
        );
        assert!(depth_limited.truncated);
        assert!(depth_limited.next_cursor.is_none());

        let complete = store
            .inspect_subtree_page(
                "inspect",
                fixture.deep_parent_id,
                16,
                PageLimit::new(100).expect("limit"),
                None,
            )
            .expect("complete deep inspection");
        assert_eq!(complete.items.len(), 17);
        assert_eq!(complete.items.last().expect("leaf").child_count, 0);
        assert!(!complete.truncated);
        assert!(complete.next_cursor.is_none());
    }

    #[test]
    fn comparison_probe_detects_repeated_linear_projection_mapping() {
        let fixture = generate_large_tree_fixture(fixture_spec(), 83);
        let requested = fixture
            .wide_child_ids
            .iter()
            .rev()
            .take(12)
            .copied()
            .collect::<Vec<_>>();
        let observation = observe_linear_lookup(&requested, &fixture.wide_child_ids);
        assert_eq!(observation.matched, requested.len());
        assert!(observation.is_repeated_linear_mapping());
        assert!(observation.comparisons > 200);
    }
}
