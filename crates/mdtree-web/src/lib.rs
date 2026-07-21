//! Local web tree viewer and editor adapter for `MDTree`.
//!
//! This crate is a thin delivery adapter, mirroring `mdtree-mcp`: it depends only
//! on `mdtree-core`, `mdtree-sqlite`, and `mdtree-markdown`, and every read or
//! mutation goes through the existing application services those crates already
//! expose. No tree, version, or path validation logic is duplicated here.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use axum::routing::{get, post};
use axum::Router;
use mdtree_core::{NodeId, NodeSelector};
use mdtree_sqlite::SqliteStore;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

mod api;
mod assets;
mod change_hub;
mod commands;
mod lifecycle;
mod markdown;
mod search;
mod security;
mod state;
mod ws;

use change_hub::CHANGE_CHANNEL_CAPACITY;
use lifecycle::ClientActivity;
use security::{enforce_same_origin, generate_session_credential};
use state::{AppState, WorkspaceState};

/// Options controlling one `browse-ui` session.
#[derive(Debug, Clone, Default)]
pub struct BrowseUiOptions {
    /// Optional subtree root selected by ID, slug, or path. Only meaningful
    /// when exactly one workspace is being served.
    pub selector: Option<String>,
    /// Open the session URL in the operating system's default browser.
    pub open_browser: bool,
    /// TCP port to bind. Port 0 asks the operating system to choose one.
    pub port: u16,
}

/// One workspace database to open, with an optional caller-supplied display
/// name overriding the name derived from its file path.
#[derive(Debug, Clone)]
pub struct WorkspaceSource {
    /// Workspace database path.
    pub path: PathBuf,
    /// Explicit display name; falls back to a name derived from `path` when
    /// absent (see `default_workspace_name`).
    pub name: Option<String>,
}

/// Derives a workspace's display name from its file path when no explicit
/// name was supplied: the file's own stem, unless that stem is generic (a
/// leading-dot file like `.mdtree`, or literally `mdtree`) — in which case
/// the containing directory's name is used instead, since a project's
/// workspace file is conventionally just `.mdtree` in its root.
fn default_workspace_name(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let is_generic =
        stem.is_empty() || stem.starts_with('.') || stem.eq_ignore_ascii_case("mdtree");
    if is_generic {
        // `path.parent()` on a bare single-segment relative path (the
        // common case: the default workspace is just `.mdtree` in the
        // current directory) returns an empty path, not `None` — it has no
        // directory component to report. Canonicalizing first resolves it
        // against the actual current directory so the fallback still finds
        // a meaningful containing-directory name instead of silently
        // falling through to the generic stem.
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                path.canonicalize()
                    .ok()
                    .and_then(|absolute| absolute.parent().map(std::path::Path::to_path_buf))
            });
        if let Some(parent_name) = parent
            .as_deref()
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
        {
            return parent_name.to_string();
        }
    }
    if stem.is_empty() {
        path.display().to_string()
    } else {
        stem.to_string()
    }
}

/// Starts a web session over `workspaces` and blocks until the session ends.
///
/// Every workspace is opened and its root resolved before the listener is
/// opened or a browser is launched, so an invalid workspace or selector
/// fails the command without ever presenting a browser window.
///
/// # Errors
///
/// Returns an error when a workspace cannot be opened, `options.selector`
/// does not resolve to an existing node, or the network listener cannot
/// bind.
///
/// # Panics
///
/// Panics if a workspace's store mutex is poisoned, which only happens after
/// an unrelated panic already unwound while holding the lock.
pub async fn run(workspaces: &[WorkspaceSource], options: BrowseUiOptions) -> anyhow::Result<()> {
    let mut workspace_states = Vec::with_capacity(workspaces.len());
    for source in workspaces {
        let store = SqliteStore::open(&source.path)?;
        // A subtree selector only makes sense for a single workspace; the
        // CLI already rejects combining `--also-workspace` with a selector.
        let selector = if workspaces.len() == 1 {
            options.selector.as_deref()
        } else {
            None
        };
        let root = resolve_root(&store, selector)?;
        let name = source
            .name
            .clone()
            .unwrap_or_else(|| default_workspace_name(&source.path));
        tracing::debug!(%root, %name, "resolved browse-ui workspace root");
        let (changes_tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        workspace_states.push(WorkspaceState {
            store: Arc::new(Mutex::new(store)),
            root,
            name,
            changes: changes_tx,
        });
    }
    // The left-hand switcher panel lists workspaces alphabetically by
    // display name rather than command-line order, so callers don't need to
    // sort `--also-workspace` arguments themselves.
    workspace_states.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });

    let listener =
        TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, options.port))).await?;
    let addr: SocketAddr = listener.local_addr()?;
    let url = format!("http://{}:{}", advertised_ipv4_address(), addr.port());
    println!("{url}");
    std::io::stdout().flush()?;

    if options.open_browser {
        open_browser(&url);
    }

    let (shutdown_tx, _) = watch::channel(false);
    let state = AppState {
        session_credential: Arc::from(generate_session_credential()),
        workspaces: Arc::new(workspace_states),
        shutdown: shutdown_tx.clone(),
        client_activity: Arc::new(ClientActivity::default()),
    };
    tracing::debug!(
        credential_len = state.session_credential.len(),
        workspace_count = state.workspaces.len(),
        "generated per-launch session credential"
    );
    tokio::spawn(lifecycle::monitor_client_activity(state.clone()));
    for (index, workspace) in state.workspaces.iter().enumerate() {
        let initial_revision = workspace
            .store
            .lock()
            .expect("workspace store mutex poisoned")
            .workspace_revision()?;
        tokio::spawn(change_hub::poll_workspace_revision(
            state.clone(),
            index,
            Arc::new(AtomicU64::new(initial_revision)),
        ));
    }
    let client_activity = Arc::clone(&state.client_activity);

    let app = Router::new()
        .route("/", get(assets::index))
        .route("/app.js", get(assets::app_js))
        .route("/style.css", get(assets::style_css))
        .route("/vendor/easymde.min.js", get(assets::easymde_js))
        .route("/vendor/easymde.min.css", get(assets::easymde_css))
        .route("/api/workspaces", get(api::workspaces))
        .route("/api/search", get(search::search))
        .route("/api/{workspace}/node/{selector}", get(api::node))
        .route("/api/{workspace}/node/{selector}/render", get(api::render))
        .route("/api/{workspace}/node/{selector}/source", get(api::source))
        .route(
            "/api/{workspace}/node/{selector}/ancestors",
            get(api::ancestors),
        )
        .route("/api/ws/{workspace}", get(ws::upgrade))
        .route("/api/stop", post(lifecycle::stop))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            enforce_same_origin,
        ))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(lifecycle::shutdown_signal(shutdown_tx))
        .await?;
    // WebSocket upgrades are handed off to detached tasks (`on_upgrade` spawns
    // them), so axum's own graceful shutdown does not wait for them. Give
    // them a bounded window to send a final "shutdown" envelope and exit
    // cleanly before this process does.
    let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while client_activity.connected() > 0 && tokio::time::Instant::now() < drain_deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Ok(())
}

fn advertised_ipv4_address() -> Ipv4Addr {
    let Ok(output) = Command::new("wg")
        .args(["show", "interfaces"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Ipv4Addr::LOCALHOST;
    };
    if !output.status.success() {
        return Ipv4Addr::LOCALHOST;
    }

    find_wireguard_ipv4(&output.stdout, interface_ipv4_address).unwrap_or(Ipv4Addr::LOCALHOST)
}

fn find_wireguard_ipv4(
    interfaces: &[u8],
    mut address_for: impl FnMut(&str) -> Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    String::from_utf8_lossy(interfaces)
        .split_whitespace()
        .find_map(|interface| {
            address_for(interface).filter(|address| is_advertisable_ipv4(*address))
        })
}

fn is_advertisable_ipv4(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_loopback()
}

#[cfg(target_os = "linux")]
fn interface_ipv4_address(interface: &str) -> Option<Ipv4Addr> {
    command_ipv4_address(
        "ip",
        &[
            "-o", "-4", "addr", "show", "dev", interface, "scope", "global",
        ],
    )
}

#[cfg(target_os = "macos")]
fn interface_ipv4_address(interface: &str) -> Option<Ipv4Addr> {
    command_ipv4_address("ipconfig", &["getifaddr", interface])
}

#[cfg(target_os = "windows")]
fn interface_ipv4_address(interface: &str) -> Option<Ipv4Addr> {
    command_ipv4_address(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-NetIPAddress -InterfaceAlias $args[0] -AddressFamily IPv4 | Select-Object -First 1 -ExpandProperty IPAddress",
            interface,
        ],
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn interface_ipv4_address(_interface: &str) -> Option<Ipv4Addr> {
    None
}

fn command_ipv4_address(command: &str, args: &[&str]) -> Option<Ipv4Addr> {
    let output = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_ipv4_address(&output.stdout))
        .flatten()
}

fn parse_ipv4_address(output: &[u8]) -> Option<Ipv4Addr> {
    String::from_utf8_lossy(output)
        .split_whitespace()
        .filter_map(|field| field.split('/').next())
        .find_map(|field| field.parse().ok())
}

fn resolve_root(store: &SqliteStore, selector: Option<&str>) -> anyhow::Result<NodeId> {
    match selector {
        Some(raw) => {
            let parsed = NodeSelector::from_str(raw)?;
            store
                .resolve(&parsed)?
                .map(|node| node.id())
                .ok_or_else(|| anyhow::anyhow!("node not found: {raw}"))
        }
        None => Ok(store.root()?.id()),
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let outcome = std::process::Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[cfg(target_os = "windows")]
    let outcome = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let outcome = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    if let Err(error) = outcome {
        tracing::warn!(%error, "failed to launch the default browser; use the printed URL instead");
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{find_wireguard_ipv4, parse_ipv4_address};

    #[test]
    fn parses_ipv4_from_linux_address_output() {
        let output = b"7: wg0    inet 10.44.0.3/24 scope global wg0\n";
        assert_eq!(
            parse_ipv4_address(output),
            Some(Ipv4Addr::new(10, 44, 0, 3))
        );
    }

    #[test]
    fn selects_the_first_wireguard_interface_with_an_advertisable_ipv4_address() {
        let address = find_wireguard_ipv4(b"wg0 wg-office\n", |interface| match interface {
            "wg0" => None,
            "wg-office" => Some(Ipv4Addr::new(10, 60, 0, 8)),
            _ => unreachable!(),
        });
        assert_eq!(address, Some(Ipv4Addr::new(10, 60, 0, 8)));
    }

    #[test]
    fn ignores_non_advertisable_wireguard_addresses() {
        assert_eq!(
            find_wireguard_ipv4(b"wg0\n", |_| Some(Ipv4Addr::LOCALHOST)),
            None
        );
    }
}
