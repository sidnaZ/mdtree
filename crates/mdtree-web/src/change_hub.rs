//! Cross-process change detection and the local broadcast change hub.
//!
//! Persisted changes made through another `browse-ui` process, the CLI, or
//! MCP must reach every interested server and then be pushed to its browser
//! clients. Browser clients never poll; a server periodically reconciles
//! from the canonical store's workspace revision counter and, on a change,
//! broadcasts a revision-aware notification to its own connected clients —
//! never a replacement snapshot. Because the counter is workspace-wide
//! rather than per-node, a change conservatively marks the client's entire
//! visible scope stale rather than guessing which node changed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::state::AppState;

/// Capacity of the local change-broadcast channel.
pub(crate) const CHANGE_CHANNEL_CAPACITY: usize = 64;
/// How often this process checks the workspace revision for changes made by
/// other writers.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One workspace-revision change, broadcast to every locally connected client.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChangeEvent {
    pub(crate) revision: u64,
}

/// Polls one workspace's revision and broadcasts a [`ChangeEvent`] to that
/// workspace's own clients whenever it advances, regardless of which
/// process or interface caused the change. One task runs per open
/// workspace, so a change in one never marks another workspace's clients
/// stale.
pub(crate) async fn poll_workspace_revision(
    state: AppState,
    workspace_index: usize,
    last_seen: Arc<AtomicU64>,
) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let workspace = &state.workspaces[workspace_index];
        let current = {
            let store = workspace
                .store
                .lock()
                .expect("workspace store mutex poisoned");
            store.workspace_revision().unwrap_or(0)
        };
        let previous = last_seen.swap(current, Ordering::SeqCst);
        if current != previous {
            // No receivers is not an error here: it just means no client is
            // connected right now to notify.
            let _ = workspace.changes.send(ChangeEvent { revision: current });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CHANGE_CHANNEL_CAPACITY;
    use tokio::sync::broadcast;

    #[test]
    fn channel_delivers_to_every_subscriber() {
        let (tx, mut a) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let mut b = tx.subscribe();
        tx.send(super::ChangeEvent { revision: 7 }).expect("send");
        assert_eq!(a.try_recv().expect("a receives").revision, 7);
        assert_eq!(b.try_recv().expect("b receives").revision, 7);
    }
}
