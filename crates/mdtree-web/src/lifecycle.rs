//! Connected-client tracking, authenticated Stop, and shutdown-signal
//! composition.
//!
//! An explicit authenticated Stop begins graceful shutdown immediately.
//! Closing one of several tabs does not stop the server; after at least one
//! WebSocket client has connected, loss of the final one starts a short
//! grace period, and a new connection within that window cancels it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use tokio::sync::watch;

use crate::state::AppState;

/// How long the session survives after the last connected client disconnects.
const CLIENT_GRACE_PERIOD: Duration = Duration::from_secs(5);
/// How often the grace-period monitor checks elapsed time.
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
/// Header carrying the per-launch session credential on authenticated requests.
pub(crate) const SESSION_HEADER: &str = "x-mdtree-session";

/// Tracks currently connected WebSocket clients and when the last one left.
#[derive(Default)]
pub(crate) struct ClientActivity {
    connected: AtomicUsize,
    ever_connected: AtomicBool,
    disconnected_at: Mutex<Option<Instant>>,
}

impl ClientActivity {
    pub(crate) fn client_connected(&self) {
        self.connected.fetch_add(1, Ordering::SeqCst);
        self.ever_connected.store(true, Ordering::SeqCst);
        *self
            .disconnected_at
            .lock()
            .expect("activity mutex poisoned") = None;
    }

    pub(crate) fn client_disconnected(&self) {
        let remaining = self
            .connected
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        if remaining == 0 {
            *self
                .disconnected_at
                .lock()
                .expect("activity mutex poisoned") = Some(Instant::now());
        }
    }

    fn should_shut_down(&self) -> bool {
        if !self.ever_connected.load(Ordering::SeqCst) || self.connected.load(Ordering::SeqCst) > 0
        {
            return false;
        }
        self.disconnected_at
            .lock()
            .expect("activity mutex poisoned")
            .is_some_and(|since| since.elapsed() > CLIENT_GRACE_PERIOD)
    }

    /// Number of currently connected WebSocket clients.
    pub(crate) fn connected(&self) -> usize {
        self.connected.load(Ordering::SeqCst)
    }
}

pub(crate) async fn stop(State(state): State<AppState>, headers: HeaderMap) -> StatusCode {
    let supplied = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok());
    if supplied != Some(&*state.session_credential) {
        return StatusCode::FORBIDDEN;
    }
    let _ = state.shutdown.send(true);
    StatusCode::ACCEPTED
}

/// Watches for last-client grace-period expiry and triggers shutdown.
pub(crate) async fn monitor_client_activity(state: AppState) {
    loop {
        tokio::time::sleep(MONITOR_INTERVAL).await;
        if state.client_activity.should_shut_down() {
            let _ = state.shutdown.send(true);
            break;
        }
    }
}

/// Resolves when Ctrl+C, `SIGTERM`, or an internal shutdown request occurs.
///
/// Whichever branch fires, the shared `shutdown` watch is marked true before
/// returning, so every connected WebSocket handler (each holding its own
/// receiver) learns about the shutdown regardless of what triggered it, not
/// only the Stop/grace-period path that already sets the watch directly.
pub(crate) async fn shutdown_signal(shutdown: watch::Sender<bool>) {
    let mut shutdown_rx = shutdown.subscribe();
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        signal.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    let shutdown_requested = async {
        while !*shutdown_rx.borrow() {
            if shutdown_rx.changed().await.is_err() {
                return;
            }
        }
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
        () = shutdown_requested => {},
    }
    let _ = shutdown.send(true);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ClientActivity;

    #[test]
    fn does_not_shut_down_before_any_client_has_connected() {
        let activity = ClientActivity::default();
        assert!(!activity.should_shut_down());
    }

    #[test]
    fn does_not_shut_down_while_a_client_remains_connected() {
        let activity = ClientActivity::default();
        activity.client_connected();
        activity.client_connected();
        activity.client_disconnected();
        assert!(!activity.should_shut_down());
    }

    #[test]
    fn shuts_down_only_after_the_grace_period_elapses_since_the_last_client_left() {
        let activity = ClientActivity::default();
        activity.client_connected();
        activity.client_disconnected();
        assert!(!activity.should_shut_down());

        *activity.disconnected_at.lock().expect("mutex") =
            std::time::Instant::now().checked_sub(Duration::from_secs(10));
        assert!(activity.should_shut_down());
    }

    #[test]
    fn a_new_connection_within_the_grace_period_cancels_the_pending_shutdown() {
        let activity = ClientActivity::default();
        activity.client_connected();
        activity.client_disconnected();
        activity.client_connected();
        assert!(!activity.should_shut_down());
    }
}
