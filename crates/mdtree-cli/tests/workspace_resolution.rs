//! End-to-end coverage for CLI workspace path precedence and defaults.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;

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

fn stop_web_ui(url: &str) {
    let address = url.strip_prefix("http://").expect("HTTP URL");
    let mut stream = TcpStream::connect(address).expect("connect to web UI");
    write!(
        stream,
        "GET /api/workspaces HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .expect("request session");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read session");
    let body = response.split("\r\n\r\n").nth(1).expect("response body");
    let session: serde_json::Value = serde_json::from_str(body).expect("session JSON");
    let credential = session["session_credential"].as_str().expect("credential");

    let mut stream = TcpStream::connect(address).expect("reconnect to web UI");
    write!(
        stream,
        "POST /api/stop HTTP/1.1\r\nHost: {address}\r\nx-mdtree-session: {credential}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .expect("stop web UI");
}

#[test]
fn commands_use_dot_mdtree_in_the_current_directory() {
    let directory = tempdir().expect("temporary directory");

    let initialized = mdtree()
        .current_dir(directory.path())
        .args(["init", "Example"])
        .output()
        .expect("run init");
    assert!(
        initialized.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(directory.path().join(".mdtree").is_file());

    let status = mdtree()
        .current_dir(directory.path())
        .arg("status")
        .output()
        .expect("run status");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

#[test]
fn environment_and_explicit_workspace_override_the_default_in_precedence_order() {
    let directory = tempdir().expect("temporary directory");

    let environment = mdtree()
        .current_dir(directory.path())
        .env("MDTREE_WORKSPACE", "environment.mdtree")
        .args(["init", "Environment"])
        .output()
        .expect("run environment init");
    assert!(environment.status.success());
    assert!(directory.path().join("environment.mdtree").is_file());
    assert!(!directory.path().join(".mdtree").exists());

    let explicit = mdtree()
        .current_dir(directory.path())
        .env("MDTREE_WORKSPACE", "ignored.mdtree")
        .args(["--workspace", "explicit.mdtree", "init", "Explicit"])
        .output()
        .expect("run explicit init");
    assert!(explicit.status.success());
    assert!(directory.path().join("explicit.mdtree").is_file());
    assert!(!directory.path().join("ignored.mdtree").exists());
}

#[test]
fn missing_default_workspace_diagnostic_names_the_path() {
    let directory = tempdir().expect("temporary directory");
    let result = mdtree()
        .current_dir(directory.path())
        .arg("status")
        .output()
        .expect("run status");

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("Could not open MDTree workspace .mdtree"),
        "{stderr}"
    );
    assert!(stderr.contains("workspace file does not exist"), "{stderr}");
    assert!(stderr.contains("Underlying error:"), "{stderr}");
    assert!(stderr.contains("path is correct"), "{stderr}");
    assert!(
        stderr.contains("mdtree init \"My Knowledge Base\""),
        "{stderr}"
    );
    assert!(
        stderr.contains("mdtree --workspace /path/to/workspace.mdtree status"),
        "{stderr}"
    );
    assert!(!directory.path().join(".mdtree").exists());
}

#[test]
fn corrupt_workspace_diagnostic_explains_reason_and_recovery() {
    let directory = tempdir().expect("temporary directory");
    std::fs::write(directory.path().join(".mdtree"), b"not sqlite data")
        .expect("write corrupt workspace");

    let result = mdtree()
        .current_dir(directory.path())
        .arg("status")
        .output()
        .expect("run status");

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("not a valid SQLite database"), "{stderr}");
    assert!(stderr.contains("may be corrupt"), "{stderr}");
    assert!(stderr.contains("delete it"), "{stderr}");
    assert!(
        stderr.contains("mdtree init \"My Knowledge Base\""),
        "{stderr}"
    );
}

#[test]
fn explicit_missing_workspace_fails_consistently_with_or_without_create() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("missing.mdtree");

    let bare = mdtree()
        .args(["--workspace", workspace.to_str().expect("UTF-8 path")])
        .output()
        .expect("run explicit bare command");
    assert!(!bare.status.success());

    let create = mdtree()
        .args([
            "--workspace",
            workspace.to_str().expect("UTF-8 path"),
            "create",
            "x",
            "y",
        ])
        .output()
        .expect("run create");
    assert!(!create.status.success());

    for stderr in [&bare.stderr, &create.stderr] {
        let stderr = String::from_utf8_lossy(stderr);
        assert!(stderr.contains("workspace file does not exist"), "{stderr}");
        assert!(
            stderr.contains("Underlying error: file does not exist"),
            "{stderr}"
        );
    }
    assert!(!workspace.exists());
}

#[test]
fn bare_command_guides_initialization_then_starts_the_web_ui() {
    let directory = tempdir().expect("temporary directory");
    let port = available_port();

    let welcome = mdtree()
        .current_dir(directory.path())
        .output()
        .expect("run onboarding");
    assert!(welcome.status.success());
    let welcome = String::from_utf8_lossy(&welcome.stdout);
    assert!(welcome.contains("No MDTree workspace found at .mdtree"));
    assert!(welcome.contains("mdtree init \"My Knowledge Base\""));
    assert!(welcome.contains("database and its root node"));

    let initialized = mdtree()
        .current_dir(directory.path())
        .args(["init", "My Knowledge Base"])
        .output()
        .expect("initialize workspace");
    assert!(initialized.status.success());

    let tree = mdtree()
        .current_dir(directory.path())
        .args(["--no-open", "--port"])
        .arg(port.to_string())
        .output()
        .expect("start web UI");
    assert!(
        tree.status.success(),
        "web UI startup failed: {}",
        String::from_utf8_lossy(&tree.stderr)
    );
    let stdout = String::from_utf8(tree.stdout).expect("UTF-8 URL");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "successful startup prints only one line");
    let address: SocketAddr = lines[0]
        .strip_prefix("http://")
        .expect("HTTP URL")
        .parse()
        .expect("IPv4 socket address");
    assert!(address.is_ipv4(), "{stdout}");
    assert_eq!(address.port(), port);
    stop_web_ui(lines[0]);
}

#[test]
fn bare_command_explains_how_to_recover_from_an_empty_file() {
    let directory = tempdir().expect("temporary directory");
    std::fs::File::create(directory.path().join(".mdtree")).expect("create empty workspace");

    let result = mdtree()
        .current_dir(directory.path())
        .output()
        .expect("run onboarding");
    assert!(result.status.success());
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("empty file"), "{stdout}");
    assert!(stdout.contains("Remove or rename it"), "{stdout}");
    assert!(
        stdout.contains("mdtree init \"My Knowledge Base\""),
        "{stdout}"
    );
}
