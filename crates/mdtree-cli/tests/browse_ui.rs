//! Real-process coverage for `mdtree browse-ui` session startup and shutdown.
//!
//! Lives here rather than in `mdtree-web` because `CARGO_BIN_EXE_mdtree` is only
//! defined for the package that owns the `[[bin]]`.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::process::{Command, Stdio};

use tempfile::tempdir;

fn mdtree() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mdtree"))
}

fn available_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve an available port")
        .local_addr()
        .expect("reserved port address")
        .port()
}

fn read_listening_url(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read startup line");
    let url = line.trim();
    assert!(url.starts_with("http://"), "startup line is only the URL");
    assert!(!url.chars().any(char::is_whitespace));
    url.to_string()
}

fn non_loopback_address(port: u16) -> Option<SocketAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback()).then(|| SocketAddr::new(ip, port))
}

/// Sends a raw `GET <path>` request against `address`, optionally with an
/// `Origin` header, and returns the status code and response body.
fn http_get(address: &str, path: &str, origin: Option<&str>) -> (u16, String) {
    http_get_with_host(address, address, path, origin)
}

fn http_get_with_host(
    address: &str,
    host: &str,
    path: &str,
    origin: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).expect("connect to the loopback listener");
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(origin) = origin {
        let _ = write!(request, "Origin: {origin}\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write request");

    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let mut parts = response.splitn(2, "\r\n\r\n");
    let head = parts.next().expect("response head");
    let body = parts.next().unwrap_or_default().to_string();
    let status_line = head.lines().next().expect("status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status code");
    (status, body)
}

#[test]
fn browse_ui_returns_after_starting_a_background_server_and_prints_only_its_url() {
    let directory = tempdir().expect("tempdir");
    let port = available_port();
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let started = mdtree()
        .current_dir(directory.path())
        .args(["browse-ui", "--no-open", "--port"])
        .arg(port.to_string())
        .output()
        .expect("start background web UI");
    assert!(started.status.success());
    assert!(started.stderr.is_empty(), "unexpected stderr");
    let stdout = String::from_utf8(started.stdout).expect("UTF-8 stdout");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "stdout must contain only the URL");
    let url = lines[0];
    let address = url.strip_prefix("http://").expect("HTTP URL");
    let socket: SocketAddr = address
        .parse()
        .expect("URL contains an IPv4 socket address");
    assert!(socket.is_ipv4(), "{url}");
    assert_eq!(socket.port(), port);

    let credential = session_credential(address);
    assert_eq!(http_post(address, "/api/stop", Some(&credential)), 202);
}

#[test]
fn browse_ui_foreground_keeps_the_invoking_process_attached_until_shutdown() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["browse-ui", "--foreground", "--no-open"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start foreground web UI");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("HTTP URL");

    assert!(
        child
            .try_wait()
            .expect("inspect foreground process")
            .is_none(),
        "foreground process returned while its server was still running"
    );
    let credential = session_credential(address);
    assert_eq!(http_post(address, "/api/stop", Some(&credential)), 202);
    assert!(child
        .wait()
        .expect("wait for foreground shutdown")
        .success());
}

/// Sends a raw `GET` request for a path with no route against `address`,
/// optionally with an `Origin` header, and returns the HTTP status code.
fn get_status(address: &str, origin: Option<&str>) -> u16 {
    http_get(address, "/no-such-route", origin).0
}

/// Fetches this session's credential, required on the WebSocket upgrade the
/// same as it is on Stop.
fn session_credential(address: &str) -> String {
    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    session["session_credential"]
        .as_str()
        .expect("credential")
        .to_string()
}

/// Sends a raw `POST <path>` request with no body, optionally carrying a
/// session-credential header, and returns the status code.
fn http_post(address: &str, path: &str, session_header: Option<&str>) -> u16 {
    let mut stream = TcpStream::connect(address).expect("connect to the loopback listener");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\nConnection: close\r\n"
    );
    if let Some(credential) = session_header {
        let _ = write!(request, "x-mdtree-session: {credential}\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write request");

    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let status_line = response.lines().next().expect("status line");
    status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status code")
}

#[test]
fn browse_ui_starts_a_loopback_session_and_can_be_terminated() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    TcpStream::connect(address).expect("connect to the loopback listener");
    let port = address
        .rsplit_once(':')
        .expect("host and port")
        .1
        .parse()
        .expect("numeric port");
    if let Some(network_address) = non_loopback_address(port) {
        TcpStream::connect(network_address)
            .expect("listener accepts connections through a non-loopback interface");
    }

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[test]
fn browse_ui_stop_requires_the_correct_session_credential_and_exits_cleanly() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let credential = session["session_credential"]
        .as_str()
        .expect("credential")
        .to_string();

    assert_eq!(
        http_post(address, "/api/stop", None),
        403,
        "Stop without a credential must be rejected"
    );
    assert_eq!(
        http_post(address, "/api/stop", Some("wrong-credential")),
        403,
        "Stop with the wrong credential must be rejected"
    );
    // The process must still be alive after both rejected Stop attempts.
    let (status, _) = http_get(address, "/api/workspaces", None);
    assert_eq!(status, 200);

    assert_eq!(
        http_post(address, "/api/stop", Some(&credential)),
        202,
        "Stop with the correct credential must be accepted"
    );
    let exit_status = child.wait().expect("wait for process exit");
    assert!(exit_status.success(), "status: {exit_status:?}");
}

#[test]
fn browse_ui_rejects_mismatched_origin_and_allows_same_origin_requests() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    assert_eq!(
        get_status(address, Some("https://evil.example")),
        403,
        "a mismatched Origin header must be rejected"
    );
    assert_eq!(
        get_status(address, Some(&url)),
        404,
        "a matching Origin header must pass through to routing"
    );
    assert_eq!(
        http_get_with_host(
            address,
            "192.0.2.10:4567",
            "/no-such-route",
            Some("http://192.0.2.10:4567")
        )
        .0,
        404,
        "same-origin requests through a non-loopback host must be accepted"
    );
    assert_eq!(
        http_get_with_host(
            address,
            "192.0.2.10:4567",
            "/no-such-route",
            Some("http://192.0.2.11:4567")
        )
        .0,
        403,
        "a different network host must remain cross-origin"
    );
    assert_eq!(
        get_status(address, None),
        404,
        "an absent Origin header (ordinary navigation) must pass through to routing"
    );

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[test]
fn browse_ui_serves_the_embedded_frontend_shell() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (status, body) = http_get(address, "/", None);
    assert_eq!(status, 200);
    assert!(body.contains("tree-canvas"), "body: {body}");

    let (status, body) = http_get(address, "/app.js", None);
    assert_eq!(status, 200);
    assert!(body.contains("function init"), "body: {body}");

    let (status, body) = http_get(address, "/style.css", None);
    assert_eq!(status, 200);
    assert!(body.contains("#tree-canvas"), "body: {body}");

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[test]
fn browse_ui_serves_session_node_and_sanitized_render_endpoints() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example Root"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    let create = mdtree()
        .current_dir(directory.path())
        .args([
            "create",
            "example-root",
            "Child",
            "--content",
            "# Child\n\n<script>alert('x')</script>\n\nsafe body",
        ])
        .output()
        .expect("run create");
    assert!(create.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (status, body) = http_get(address, "/api/workspaces", None);
    assert_eq!(status, 200);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    assert!(!session["session_credential"]
        .as_str()
        .expect("credential")
        .is_empty());

    let (status, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    assert_eq!(status, 200);
    let node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    assert_eq!(node["title"], "Example Root");
    assert_eq!(node["path"], "example-root");
    assert_eq!(node["children"].as_array().expect("children").len(), 1);
    assert_eq!(node["children"][0]["path"], "example-root/child");
    let child_id = node["children"][0]["id"].as_str().expect("child id");

    let (status, body) = http_get(address, &format!("/api/0/node/{child_id}/render"), None);
    assert_eq!(status, 200);
    let rendered: serde_json::Value = serde_json::from_str(&body).expect("render JSON");
    let html = rendered["html"].as_str().expect("html");
    assert!(!html.contains("<script"), "html: {html}");
    assert!(html.contains("safe body"), "html: {html}");

    let (status, _) = http_get(address, "/api/0/node/not-a-real-node", None);
    assert_eq!(status, 404);

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[test]
fn browse_ui_serves_outgoing_references_with_target_details_and_an_ancestors_endpoint() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example Root"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    let create = mdtree()
        .current_dir(directory.path())
        .args(["create", "example-root", "Child", "--content", "# Child"])
        .output()
        .expect("run create");
    assert!(create.status.success());
    let reference_add = mdtree()
        .current_dir(directory.path())
        .args([
            "reference-add",
            "example-root/child",
            "example-root",
            "depends_on",
            "--expected-version",
            "1",
        ])
        .output()
        .expect("run reference-add");
    assert!(reference_add.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (status, body) = http_get(address, "/api/workspaces", None);
    assert_eq!(status, 200);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();

    let (status, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    assert_eq!(status, 200);
    let node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let child_summary = &node["children"][0];
    let child_id = child_summary["id"].as_str().expect("child id").to_string();
    let references = child_summary["references"]
        .as_array()
        .expect("references array");
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["reference_type"], "depends_on");
    assert_eq!(references[0]["status"], "resolved");
    assert_eq!(references[0]["node_id"], root_id);
    assert_eq!(references[0]["title"], "Example Root");
    assert_eq!(references[0]["path"], "example-root");

    let (status, body) = http_get(address, &format!("/api/0/node/{child_id}/ancestors"), None);
    assert_eq!(status, 200);
    let ancestors: serde_json::Value = serde_json::from_str(&body).expect("ancestors JSON");
    assert_eq!(
        ancestors["ancestor_ids"].as_array().expect("ancestor_ids"),
        &vec![serde_json::Value::String(root_id.clone())]
    );

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[test]
fn browse_ui_fails_before_binding_a_listener_when_the_selector_is_unknown() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let output = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui", "no-such-node"])
        .output()
        .expect("run browse-ui");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("node not found"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).is_empty(),
        "no listening URL should be printed on startup failure"
    );
}

/// The realtime channel grants live structural-mutation authority, so it
/// must require the session credential the same as Stop does — same-origin
/// enforcement alone is not enough, since it only rejects a request that
/// *carries* a mismatched `Origin` header and lets one with none through.
/// Uses a real WebSocket handshake (not a plain GET) so the failure is
/// actually attributable to the credential check inside the handler rather
/// than to axum's own upgrade-header validation.
#[tokio::test]
async fn browse_ui_websocket_upgrade_rejects_a_missing_or_incorrect_session_credential() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Error as WsError;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let handshake_status = |path: String| async move {
        let request = format!("ws://{address}{path}")
            .into_client_request()
            .expect("valid websocket request");
        match tokio_tungstenite::connect_async(request).await {
            Ok(_) => None,
            Err(WsError::Http(response)) => Some(response.status().as_u16()),
            Err(other) => panic!("unexpected handshake failure: {other}"),
        }
    };

    assert_eq!(
        handshake_status("/api/ws/0".to_string()).await,
        Some(400),
        "an upgrade request with no session query parameter at all must be rejected"
    );
    assert_eq!(
        handshake_status("/api/ws/0?session=not-the-real-credential".to_string()).await,
        Some(403),
        "an upgrade request with the wrong session credential must be rejected"
    );

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
async fn browse_ui_websocket_sends_init_then_shutdown_on_stop() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let credential = session["session_credential"]
        .as_str()
        .expect("credential")
        .to_string();

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));

    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");

    let first = socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error");
    let WsMessage::Text(text) = first else {
        panic!("expected a text frame, got {first:?}");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "init");
    assert_eq!(envelope["v"], 1);
    assert_eq!(
        envelope["payload"]["root"],
        session["workspaces"][0]["root"].clone()
    );
    assert!(envelope["revision"].is_u64());

    assert_eq!(
        http_post(address, "/api/stop", Some(&credential)),
        202,
        "Stop with the correct credential must be accepted"
    );

    let mut saw_shutdown = false;
    while let Some(message) = socket.next().await {
        let WsMessage::Text(text) = message.expect("no websocket error") else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
        if envelope["type"] == "shutdown" {
            saw_shutdown = true;
            break;
        }
    }
    assert!(
        saw_shutdown,
        "expected a shutdown envelope before the socket closed"
    );

    let exit_status = child.wait().expect("wait for process exit");
    assert!(exit_status.success(), "status: {exit_status:?}");
}

/// The other half of "clean owned-process termination" (the first half is
/// covered above via Stop): closing the only connected client, with no Stop
/// call at all, must also end the process on its own once the grace period
/// elapses — not immediately, and not by hanging forever.
#[tokio::test]
async fn browse_ui_terminates_on_its_own_after_the_last_client_disconnects_and_the_grace_period_elapses(
) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");

    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error");

    // Disconnect the only client without ever calling Stop.
    drop(socket);

    let started = std::time::Instant::now();
    let exit_status = child.wait().expect("wait for process exit");
    let elapsed = started.elapsed();
    assert!(exit_status.success(), "status: {exit_status:?}");
    // The grace period (5s) must actually have been honored, not skipped —
    // an immediate exit here would mean any tab closing an accidental extra
    // connection (e.g. a page reload) could kill the session for everyone.
    assert!(
        elapsed >= std::time::Duration::from_secs(4),
        "exited after only {elapsed:?}, grace period was not honored"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "took {elapsed:?} to exit after the grace period; expected well under 15s"
    );
}

#[tokio::test]
async fn browse_ui_notifies_its_clients_when_another_process_mutates_the_shared_workspace() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Shared Workspace"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");

    let first = socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error");
    let WsMessage::Text(text) = first else {
        panic!("expected a text frame, got {first:?}");
    };
    let init_envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    let initial_revision = init_envelope["revision"].as_u64().expect("revision");

    // A completely independent writer — the plain CLI, not this browse-ui
    // process — mutates the same on-disk workspace directly.
    let create = mdtree()
        .current_dir(directory.path())
        .args([
            "create",
            "shared-workspace",
            "Created While Watching",
            "--content",
            "# Created While Watching",
        ])
        .output()
        .expect("run create");
    assert!(create.status.success());

    let mut saw_change = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Some(message)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await
        else {
            continue;
        };
        let WsMessage::Text(text) = message.expect("no websocket error") else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
        if envelope["type"] == "change" {
            let revision = envelope["revision"].as_u64().expect("revision");
            assert!(revision > initial_revision);
            saw_change = true;
            break;
        }
    }
    assert!(
        saw_change,
        "expected a change notification after another process mutated the shared workspace"
    );

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn browse_ui_websocket_reorder_command_applies_and_rejects_stale_versions() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Reorder Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    for title in ["Alpha", "Beta", "Gamma"] {
        let create = mdtree()
            .current_dir(directory.path())
            .args([
                "create",
                "reorder-test",
                title,
                "--content",
                &format!("# {title}"),
            ])
            .output()
            .expect("run create");
        assert!(create.status.success());
    }
    let reference_add = mdtree()
        .current_dir(directory.path())
        .args([
            "reference-add",
            "reorder-test/alpha",
            "reorder-test/gamma",
            "depends_on",
            "--expected-version",
            "1",
        ])
        .output()
        .expect("run reference-add");
    assert!(reference_add.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let children = root_node["children"].as_array().expect("children");
    assert_eq!(
        children
            .iter()
            .map(|c| c["title"].clone())
            .collect::<Vec<_>>(),
        vec!["Alpha", "Beta", "Gamma"]
    );
    let alpha_id = children[0]["id"].as_str().expect("alpha id").to_string();
    let alpha_version = children[0]["version"].as_u64().expect("alpha version");
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error"); // init

    // A stale expected_version must be rejected without changing anything.
    let stale_command = serde_json::json!({
        "v": 1,
        "id": "test-1",
        "session": "test",
        "type": "command",
        "payload": {
            "command": "reorder_node",
            "selector": alpha_id,
            "sibling_order": 2,
            "expected_version": alpha_version + 100,
        },
    });
    socket
        .send(WsMessage::Text(stale_command.to_string().into()))
        .await
        .expect("send stale reorder command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "reject");
    assert_eq!(envelope["payload"]["command"], "reorder_node");
    assert!(envelope["payload"]["reason"]
        .as_str()
        .expect("reason")
        .contains("version"));

    // Move Alpha into the intermediate slot. This deliberately requests an
    // index already occupied by Beta before insertion; node IDs must not
    // influence the result.
    let command = serde_json::json!({
        "v": 1,
        "id": "test-2",
        "session": "test",
        "type": "command",
        "payload": {
            "command": "reorder_node",
            "selector": alpha_id,
            "sibling_order": 1,
            "expected_version": alpha_version,
        },
    });
    socket
        .send(WsMessage::Text(command.to_string().into()))
        .await
        .expect("send reorder command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "ack");
    assert_eq!(envelope["payload"]["command"], "reorder_node");
    assert!(envelope["revision"].is_u64());

    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let titles: Vec<_> = root_node["children"]
        .as_array()
        .expect("children")
        .iter()
        .map(|c| c["title"].clone())
        .collect();
    assert_eq!(titles, vec!["Beta", "Alpha", "Gamma"]);
    let alpha = root_node["children"]
        .as_array()
        .expect("children")
        .iter()
        .find(|child| child["title"] == "Alpha")
        .expect("Alpha child");
    let references = alpha["references"].as_array().expect("references");
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["reference_type"], "depends_on");
    assert_eq!(references[0]["title"], "Gamma");

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn browse_ui_websocket_move_subtree_command_applies_and_rejects_cycles() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Move Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    for title in ["Alpha", "Beta"] {
        let create = mdtree()
            .current_dir(directory.path())
            .args([
                "create",
                "move-test",
                title,
                "--content",
                &format!("# {title}"),
            ])
            .output()
            .expect("run create");
        assert!(create.status.success());
    }
    let grandchild = mdtree()
        .current_dir(directory.path())
        .args([
            "create",
            "alpha",
            "Alpha Child",
            "--content",
            "# Alpha Child",
        ])
        .output()
        .expect("run create");
    assert!(grandchild.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let children = root_node["children"].as_array().expect("children");
    let alpha = children
        .iter()
        .find(|c| c["title"] == "Alpha")
        .expect("alpha");
    let beta = children
        .iter()
        .find(|c| c["title"] == "Beta")
        .expect("beta");
    let alpha_id = alpha["id"].as_str().expect("alpha id").to_string();
    let alpha_version = alpha["version"].as_u64().expect("alpha version");
    let beta_id = beta["id"].as_str().expect("beta id").to_string();

    let (_, body) = http_get(address, &format!("/api/0/node/{alpha_id}"), None);
    let alpha_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let alpha_child_id = alpha_node["children"][0]["id"]
        .as_str()
        .expect("alpha child id")
        .to_string();
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error"); // init

    // Moving Alpha below its own child must be rejected as a cycle.
    let cyclic = serde_json::json!({
        "v": 1, "id": "t1", "session": "test", "type": "command",
        "payload": {
            "command": "move_subtree",
            "selector": alpha_id,
            "new_parent": alpha_child_id,
            "expected_version": alpha_version,
        },
    });
    socket
        .send(WsMessage::Text(cyclic.to_string().into()))
        .await
        .expect("send cyclic move command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "reject");
    assert_eq!(envelope["payload"]["command"], "move_subtree");

    // Moving Alpha under Beta is valid and commits one canonical move.
    let valid = serde_json::json!({
        "v": 1, "id": "t2", "session": "test", "type": "command",
        "payload": {
            "command": "move_subtree",
            "selector": alpha_id,
            "new_parent": beta_id,
            "expected_version": alpha_version,
        },
    });
    socket
        .send(WsMessage::Text(valid.to_string().into()))
        .await
        .expect("send move command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "ack");
    assert_eq!(envelope["payload"]["command"], "move_subtree");

    let tree = mdtree()
        .current_dir(directory.path())
        .args(["tree", "--output", "json"])
        .output()
        .expect("run tree");
    assert!(tree.status.success());
    let nodes: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&tree.stdout)).expect("tree JSON");
    let moved = nodes
        .as_array()
        .expect("tree array")
        .iter()
        .find(|entry| entry["node"]["id"] == alpha_id)
        .expect("moved node present");
    assert_eq!(moved["node"]["parent_id"], beta_id);

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
async fn browse_ui_rejects_a_command_racing_a_concurrent_mutation_and_marks_it_stale() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Conflict Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    let create = mdtree()
        .current_dir(directory.path())
        .args(["create", "conflict-test", "Alpha", "--content", "# Alpha"])
        .output()
        .expect("run create");
    assert!(create.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let alpha_id = root_node["children"][0]["id"]
        .as_str()
        .expect("alpha id")
        .to_string();
    // The version a drag would have captured when it began.
    let observed_version = root_node["children"][0]["version"]
        .as_u64()
        .expect("alpha version");
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error");

    // A completely independent writer mutates Alpha between when the drag
    // observed its version and when the drop is sent — simulating exactly
    // the race the drag's expected_version precondition exists to catch.
    let rename = mdtree()
        .current_dir(directory.path())
        .args([
            "rename",
            &alpha_id,
            "Alpha Renamed Concurrently",
            "--expected-version",
            &observed_version.to_string(),
        ])
        .output()
        .expect("run rename");
    assert!(rename.status.success());

    let stale_drop = serde_json::json!({
        "v": 1, "id": "t1", "session": "test", "type": "command",
        "payload": {
            "command": "reorder_node",
            "selector": alpha_id,
            "sibling_order": 0,
            "expected_version": observed_version,
        },
    });
    socket
        .send(WsMessage::Text(stale_drop.to_string().into()))
        .await
        .expect("send stale reorder command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "reject");
    assert_eq!(envelope["payload"]["command"], "reorder_node");

    // The server must never have overwritten the concurrent rename.
    let (_, body) = http_get(address, &format!("/api/0/node/{alpha_id}"), None);
    let alpha_after: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    assert_eq!(alpha_after["title"], "Alpha Renamed Concurrently");

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_and_read_init(address: &str, origin: &str) -> WsStream {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let credential = session_credential(address);
    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", origin.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error");
    socket
}

async fn await_change_notification(socket: &mut WsStream, label: &str) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Some(message)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await
        else {
            continue;
        };
        let WsMessage::Text(text) = message.expect("no websocket error") else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
        if envelope["type"] == "change" {
            return;
        }
    }
    panic!("{label} never received a change notification");
}

#[tokio::test]
async fn browse_ui_pushes_a_cli_mutation_to_two_independent_server_processes() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Multi Process Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    let create = mdtree()
        .current_dir(directory.path())
        .args([
            "create",
            "multi-process-test",
            "Watched",
            "--content",
            "# Watched",
        ])
        .output()
        .expect("run create");
    assert!(create.status.success());

    // Two independent `browse-ui` processes open the same on-disk workspace
    // on different (OS-assigned) ports.
    let mut server_a = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui A");
    let mut reader_a = BufReader::new(server_a.stdout.take().expect("captured stdout"));
    let url_a = read_listening_url(&mut reader_a);
    let address_a = url_a.strip_prefix("http://").expect("loopback URL");

    let mut server_b = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui B");
    let mut reader_b = BufReader::new(server_b.stdout.take().expect("captured stdout"));
    let url_b = read_listening_url(&mut reader_b);
    let address_b = url_b.strip_prefix("http://").expect("loopback URL");

    assert_ne!(
        address_a, address_b,
        "each process must own a distinct port"
    );

    let mut client_a = connect_and_read_init(address_a, &url_a).await;
    let mut client_b = connect_and_read_init(address_b, &url_b).await;

    // A third, unrelated writer — the plain CLI — mutates the shared
    // workspace directly; neither server process is the one that wrote it.
    let update = mdtree()
        .current_dir(directory.path())
        .args([
            "update",
            "watched",
            "--content",
            "# Watched\n\nUpdated by a third process.",
            "--expected-version",
            "1",
        ])
        .output()
        .expect("run update");
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );

    await_change_notification(&mut client_a, "server A's client").await;
    await_change_notification(&mut client_b, "server B's client").await;

    server_a.kill().expect("terminate server A");
    server_a.wait().expect("wait for server A exit");
    server_b.kill().expect("terminate server B");
    server_b.wait().expect("wait for server B exit");
}

#[test]
fn browse_ui_serves_the_source_endpoint_with_raw_markdown_and_version() {
    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Source Endpoint Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    let create = mdtree()
        .current_dir(directory.path())
        .args([
            "create",
            "source-endpoint-test",
            "Child",
            "--content",
            "# Child\n\n<script>alert('x')</script>\n\nraw body",
        ])
        .output()
        .expect("run create");
    assert!(create.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let child_id = root_node["children"][0]["id"].as_str().expect("child id");

    let (status, body) = http_get(address, &format!("/api/0/node/{child_id}/source"), None);
    assert_eq!(status, 200);
    let source: serde_json::Value = serde_json::from_str(&body).expect("source JSON");
    // Unlike /render, this is the raw Markdown — no sanitization, so the
    // literal `<script>` text is expected to still be present.
    assert!(source["markdown_content"]
        .as_str()
        .expect("markdown_content")
        .contains("<script>alert('x')</script>"));
    assert_eq!(source["version"], 1);

    let (status, _) = http_get(address, "/api/0/node/not-a-real-node/source", None);
    assert_eq!(status, 404);

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn browse_ui_websocket_update_node_command_applies_and_rejects_stale_versions() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Update Node Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());
    let create = mdtree()
        .current_dir(directory.path())
        .args([
            "create",
            "update-node-test",
            "Doc",
            "--content",
            "# Doc\n\noriginal body",
        ])
        .output()
        .expect("run create");
    assert!(create.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let doc_id = root_node["children"][0]["id"]
        .as_str()
        .expect("doc id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{doc_id}/source"), None);
    let source: serde_json::Value = serde_json::from_str(&body).expect("source JSON");
    let doc_version = source["version"].as_u64().expect("doc version");
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error"); // init

    // A stale expected_version must be rejected without changing anything.
    let stale_command = serde_json::json!({
        "v": 1,
        "id": "test-1",
        "session": "test",
        "type": "command",
        "payload": {
            "command": "update_node",
            "selector": doc_id,
            "content": "# Doc\n\nshould not be applied",
            "expected_version": doc_version + 100,
        },
    });
    socket
        .send(WsMessage::Text(stale_command.to_string().into()))
        .await
        .expect("send stale update command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "reject");
    assert_eq!(envelope["payload"]["command"], "update_node");
    assert!(envelope["payload"]["reason"]
        .as_str()
        .expect("reason")
        .contains("version"));

    let command = serde_json::json!({
        "v": 1,
        "id": "test-2",
        "session": "test",
        "type": "command",
        "payload": {
            "command": "update_node",
            "selector": doc_id,
            "content": "# Doc\n\nedited body",
            "expected_version": doc_version,
        },
    });
    socket
        .send(WsMessage::Text(command.to_string().into()))
        .await
        .expect("send update command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "ack");
    assert_eq!(envelope["payload"]["command"], "update_node");
    assert_eq!(envelope["revision"], doc_version + 1);

    let (_, body) = http_get(address, &format!("/api/0/node/{doc_id}/source"), None);
    let source: serde_json::Value = serde_json::from_str(&body).expect("source JSON");
    assert_eq!(source["markdown_content"], "# Doc\n\nedited body");
    assert_eq!(source["version"], doc_version + 1);
    // The title (and every other metadata field) must be carried forward
    // unchanged — this command only ever replaces the Markdown body.
    let (_, body) = http_get(address, &format!("/api/0/node/{doc_id}"), None);
    let node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    assert_eq!(node["title"], "Doc");

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn browse_ui_websocket_create_node_command_creates_a_child_and_returns_its_id() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Create Node Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error"); // init

    // An unknown parent selector must be rejected.
    let bad_command = serde_json::json!({
        "v": 1,
        "id": "test-1",
        "session": "test",
        "type": "command",
        "payload": {
            "command": "create_node",
            "parent": "not-a-real-node",
            "title": "Orphan",
        },
    });
    socket
        .send(WsMessage::Text(bad_command.to_string().into()))
        .await
        .expect("send bad create command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "reject");
    assert_eq!(envelope["payload"]["command"], "create_node");

    let command = serde_json::json!({
        "v": 1,
        "id": "test-2",
        "session": "test",
        "type": "command",
        "payload": {
            "command": "create_node",
            "parent": root_id,
            "title": "New Child",
        },
    });
    socket
        .send(WsMessage::Text(command.to_string().into()))
        .await
        .expect("send create command");
    let response = socket
        .next()
        .await
        .expect("response")
        .expect("no websocket error");
    let WsMessage::Text(text) = response else {
        panic!("expected a text frame");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("envelope JSON");
    assert_eq!(envelope["type"], "ack");
    assert_eq!(envelope["payload"]["command"], "create_node");
    assert_eq!(envelope["revision"], 1);
    let new_id = envelope["payload"]["node_id"]
        .as_str()
        .expect("node_id in ack payload")
        .to_string();

    let (status, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    assert_eq!(status, 200);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    let children = root_node["children"].as_array().expect("children");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], new_id);
    assert_eq!(children[0]["title"], "New Child");

    // Omitted content defaults to a level-one heading of the title.
    let (_, body) = http_get(address, &format!("/api/0/node/{new_id}/source"), None);
    let source: serde_json::Value = serde_json::from_str(&body).expect("source JSON");
    assert_eq!(source["markdown_content"], "# New Child\n");
    assert_eq!(source["version"], 1);

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn browse_ui_websocket_create_node_command_honors_an_explicit_slug_and_rejects_bad_ones() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let directory = tempdir().expect("tempdir");
    let init = mdtree()
        .current_dir(directory.path())
        .args(["init", "Create Node Slug Test"])
        .output()
        .expect("run init");
    assert!(init.status.success());

    let mut child = mdtree()
        .current_dir(directory.path())
        .args(["__serve-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn browse-ui");
    let mut reader = BufReader::new(child.stdout.take().expect("captured stdout"));
    let url = read_listening_url(&mut reader);
    let address = url.strip_prefix("http://").expect("loopback URL");

    let (_, body) = http_get(address, "/api/workspaces", None);
    let session: serde_json::Value = serde_json::from_str(&body).expect("session JSON");
    let root_id = session["workspaces"][0]["root"]
        .as_str()
        .expect("root id")
        .to_string();
    let credential = session_credential(address);

    let mut request = format!("ws://{address}/api/ws/0?session={credential}")
        .into_client_request()
        .expect("valid websocket request");
    request
        .headers_mut()
        .insert("Origin", url.parse().expect("origin header value"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket handshake");
    socket
        .next()
        .await
        .expect("init message")
        .expect("no websocket error"); // init

    async fn send_and_recv(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        id: &str,
        payload: serde_json::Value,
    ) -> serde_json::Value {
        let command = serde_json::json!({
            "v": 1,
            "id": id,
            "session": "test",
            "type": "command",
            "payload": payload,
        });
        socket
            .send(WsMessage::Text(command.to_string().into()))
            .await
            .expect("send command");
        let response = socket
            .next()
            .await
            .expect("response")
            .expect("no websocket error");
        let WsMessage::Text(text) = response else {
            panic!("expected a text frame");
        };
        serde_json::from_str(&text).expect("envelope JSON")
    }

    // An explicit slug that doesn't match the title is honored verbatim.
    let envelope = send_and_recv(
        &mut socket,
        "test-1",
        serde_json::json!({
            "command": "create_node",
            "parent": root_id,
            "title": "First Child",
            "slug": "custom-slug",
        }),
    )
    .await;
    assert_eq!(envelope["type"], "ack");
    let first_id = envelope["payload"]["node_id"]
        .as_str()
        .expect("node_id")
        .to_string();
    let (_, body) = http_get(address, &format!("/api/0/node/{first_id}"), None);
    let node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    assert_eq!(node["path"], "create-node-slug-test/custom-slug");

    // An invalid slug (uppercase) is rejected without creating anything.
    let envelope = send_and_recv(
        &mut socket,
        "test-2",
        serde_json::json!({
            "command": "create_node",
            "parent": root_id,
            "title": "Bad Slug Child",
            "slug": "Not-Valid",
        }),
    )
    .await;
    assert_eq!(envelope["type"], "reject");
    assert_eq!(envelope["payload"]["command"], "create_node");

    // A slug colliding with an existing sibling's is rejected, never
    // silently suffixed — the whole point of an explicit slug is that the
    // user's exact choice is respected or clearly refused.
    let envelope = send_and_recv(
        &mut socket,
        "test-3",
        serde_json::json!({
            "command": "create_node",
            "parent": root_id,
            "title": "Second Child",
            "slug": "custom-slug",
        }),
    )
    .await;
    assert_eq!(envelope["type"], "reject");
    assert!(envelope["payload"]["reason"]
        .as_str()
        .expect("reason")
        .contains("custom-slug"));

    let (_, body) = http_get(address, &format!("/api/0/node/{root_id}"), None);
    let root_node: serde_json::Value = serde_json::from_str(&body).expect("node JSON");
    assert_eq!(
        root_node["children"].as_array().expect("children").len(),
        1,
        "only the first, successfully-slugged child should exist"
    );

    child.kill().expect("terminate the session process");
    child.wait().expect("wait for process exit");
}
