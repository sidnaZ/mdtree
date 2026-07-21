//! Shared application state available to every route and middleware layer.

use std::sync::{Arc, Mutex};

use mdtree_core::NodeId;
use mdtree_sqlite::SqliteStore;
use tokio::sync::{broadcast, watch};

use crate::change_hub::ChangeEvent;
use crate::lifecycle::ClientActivity;

/// One open workspace within a `browse-ui` session, identified by its
/// position in [`AppState::workspaces`].
#[derive(Clone)]
pub(crate) struct WorkspaceState {
    /// The open workspace, guarded for shared access across requests.
    pub(crate) store: Arc<Mutex<SqliteStore>>,
    /// The resolved initial subtree root for this workspace.
    pub(crate) root: NodeId,
    /// Display label shown in the workspace-switcher panel: the workspace's
    /// root node title.
    pub(crate) name: String,
    /// Broadcasts a revision-aware notification to every locally connected
    /// client of this workspace whenever its revision advances, including
    /// changes made by another process, the CLI, or MCP.
    pub(crate) changes: broadcast::Sender<ChangeEvent>,
}

/// State shared across every route and middleware layer of one `browse-ui`
/// session. A session may serve several workspaces at once, switchable from
/// a panel in the web UI; the set of workspaces is fixed for the life of the
/// process.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Per-launch credential required for mutating requests, WebSocket setup,
    /// and Stop.
    pub(crate) session_credential: Arc<str>,
    /// Every workspace open in this session, in launch order. The order is
    /// also each workspace's stable id, used in URLs and the switcher panel.
    pub(crate) workspaces: Arc<Vec<WorkspaceState>>,
    /// Set to `true` to begin orderly shutdown, from Stop or grace-period
    /// expiry. A `watch` (not `Notify`) so a WebSocket connection opened
    /// after shutdown was already requested can still observe it instead of
    /// waiting forever for an edge-triggered signal it missed.
    pub(crate) shutdown: watch::Sender<bool>,
    /// Tracks connected WebSocket clients, across every workspace, for the
    /// last-client-disconnect grace period.
    pub(crate) client_activity: Arc<ClientActivity>,
}
