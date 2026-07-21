//! WebSocket upgrade and the versioned realtime protocol envelope.
//!
//! Every message uses one envelope with a message id, session (connection)
//! id, type, payload, and — where relevant — a workspace revision. Server to
//! client message types are `init`, `heartbeat`, `change`, and `shutdown`.
//! Client to server `command` envelopes carry structural mutations (see
//! [`crate::commands`]) and receive an `ack` or `reject` in response.
//! Unknown protocol versions or message types fail closed rather than being
//! guessed at.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use ulid::Ulid;

use crate::state::{AppState, WorkspaceState};

/// Query parameters accepted on the upgrade request. The session credential
/// travels here rather than as a header because the browser `WebSocket`
/// constructor cannot set custom request headers.
#[derive(Deserialize)]
pub(crate) struct UpgradeQuery {
    session: String,
}

/// Protocol version understood by this server.
const PROTOCOL_VERSION: u32 = 1;
/// Interval between server-initiated heartbeat messages.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Serialize)]
struct Envelope {
    v: u32,
    id: String,
    session: String,
    #[serde(rename = "type")]
    kind: &'static str,
    payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
}

impl Envelope {
    fn new(
        session: &str,
        kind: &'static str,
        payload: serde_json::Value,
        revision: Option<u64>,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: Ulid::gen().to_string(),
            session: session.to_string(),
            kind,
            payload,
            revision,
        }
    }
}

pub(crate) async fn upgrade(
    State(state): State<AppState>,
    Path(workspace_id): Path<usize>,
    Query(query): Query<UpgradeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // Same-origin enforcement alone is not sufficient here: it only rejects
    // a request that *carries* a mismatched `Origin` header, and explicitly
    // lets one with no `Origin` header through (ordinary top-level
    // navigation does not send one) — trivial for any non-browser local
    // client to omit. The realtime channel grants live structural-mutation
    // authority, so it needs its own credential check, same as Stop.
    if query.session != *state.session_credential {
        return (StatusCode::FORBIDDEN, "invalid session credential").into_response();
    }
    let Some(workspace) = state.workspaces.get(workspace_id).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown workspace").into_response();
    };
    ws.on_upgrade(move |socket| handle_connection(socket, state, workspace))
}

async fn handle_connection(mut socket: WebSocket, state: AppState, workspace: WorkspaceState) {
    let connection_id = Ulid::gen().to_string();

    if *state.shutdown.borrow() {
        close_for_shutdown(&mut socket, &connection_id).await;
        return;
    }

    state.client_activity.client_connected();
    let revision = current_revision(&workspace);
    let init = Envelope::new(
        &connection_id,
        "init",
        serde_json::json!({ "root": workspace.root.to_string() }),
        Some(revision),
    );
    if send(&mut socket, &init).await.is_err() {
        state.client_activity.client_disconnected();
        return;
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick; `init` just played that role
    let mut shutdown = state.shutdown.subscribe();

    // `subscribe()` starts a receiver already caught up to the current value,
    // so if shutdown was requested between the check above and here, this
    // receiver's `changed()` would otherwise never resolve (nothing will
    // change again). Handle an already-true value explicitly instead of
    // entering the wait loop.
    if *shutdown.borrow() {
        close_for_shutdown(&mut socket, &connection_id).await;
        state.client_activity.client_disconnected();
        return;
    }

    let mut changes = workspace.changes.subscribe();

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let message = Envelope::new(&connection_id, "heartbeat", serde_json::json!({}), None);
                if send(&mut socket, &message).await.is_err() {
                    break;
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    close_for_shutdown(&mut socket, &connection_id).await;
                    break;
                }
            }
            change = changes.recv() => {
                // The revision counter is workspace-wide, not per-node, so a
                // change is reported without a specific node id: the client
                // conservatively marks its whole visible scope stale rather
                // than guessing which node changed.
                let revision = match change {
                    Ok(event) => Some(event.revision),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let message = Envelope::new(&connection_id, "change", serde_json::json!({}), revision);
                if send(&mut socket, &message).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(Message::Text(text))) => {
                        if let Some(outcome) = crate::commands::handle(&text, &workspace) {
                            let kind = if outcome.ok { "ack" } else { "reject" };
                            let payload = serde_json::json!({
                                "command": outcome.command,
                                "reason": outcome.reason,
                                "node_id": outcome.node_id,
                            });
                            let message = Envelope::new(&connection_id, kind, payload, outcome.version);
                            if send(&mut socket, &message).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(_)) => {
                        // Non-text frames (ping/pong/binary) need no application response.
                    }
                }
            }
        }
    }

    state.client_activity.client_disconnected();
}

async fn close_for_shutdown(socket: &mut WebSocket, connection_id: &str) {
    let message = Envelope::new(connection_id, "shutdown", serde_json::json!({}), None);
    let _ = send(socket, &message).await;
    let _ = socket.send(Message::Close(None)).await;
}

fn current_revision(workspace: &WorkspaceState) -> u64 {
    workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned")
        .workspace_revision()
        .unwrap_or(0)
}

async fn send(socket: &mut WebSocket, envelope: &Envelope) -> Result<(), axum::Error> {
    let text = serde_json::to_string(envelope).expect("envelope always serializes");
    socket.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    #[tokio::test]
    async fn a_receiver_subscribed_after_shutdown_already_true_sees_it_without_waiting() {
        let (tx, _rx) = watch::channel(false);
        tx.send(true).expect("send");
        let subscribed_after = tx.subscribe();
        assert!(*subscribed_after.borrow());
    }
}
