//! Client-issued structural mutation commands received over the WebSocket
//! connection. Every command goes through the same `prepare_node_mutation` +
//! `SqliteStore` mutation path already used identically by `mdtree-cli` and
//! `mdtree-mcp` — no new domain logic. The server remains authoritative: it
//! validates every command against the version observed when the client's
//! drag began and never overwrites newer canonical state.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use mdtree_core::{NodeId, NodeMetadata, NodeSelector, Slug, SystemUlidGenerator, UlidGenerator};
use mdtree_sqlite::{prepare_node_mutation, NodeMutationDraft};
use serde::Deserialize;
use serde_json::Value;

use crate::state::WorkspaceState;

/// Result of processing one incoming command, sent back as an `ack` or
/// `reject` envelope.
pub(crate) struct CommandOutcome {
    pub(crate) command: String,
    pub(crate) ok: bool,
    pub(crate) reason: Option<String>,
    pub(crate) version: Option<u64>,
    /// Populated only by `create_node`'s ack, so the client can select and
    /// center the new node without having to guess its server-generated id.
    pub(crate) node_id: Option<String>,
}

impl CommandOutcome {
    fn ack(command: String, version: u64) -> Self {
        Self {
            command,
            ok: true,
            reason: None,
            version: Some(version),
            node_id: None,
        }
    }

    fn created(command: String, version: u64, node_id: String) -> Self {
        Self {
            command,
            ok: true,
            reason: None,
            version: Some(version),
            node_id: Some(node_id),
        }
    }

    fn reject(command: String, reason: String) -> Self {
        Self {
            command,
            ok: false,
            reason: Some(reason),
            version: None,
            node_id: None,
        }
    }

    /// `remove_node`'s ack: the node is gone, so — unlike `ack` — there is no
    /// new version of it to report.
    fn removed(command: String) -> Self {
        Self {
            command,
            ok: true,
            reason: None,
            version: None,
            node_id: None,
        }
    }
}

#[derive(Deserialize)]
struct IncomingEnvelope {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(Deserialize)]
struct ReorderNodePayload {
    selector: String,
    sibling_order: u32,
    expected_version: u64,
}

#[derive(Deserialize)]
struct MoveSubtreePayload {
    selector: String,
    new_parent: String,
    expected_version: u64,
}

#[derive(Deserialize)]
struct UpdateNodePayload {
    selector: String,
    content: String,
    expected_version: u64,
}

#[derive(Deserialize)]
struct RemoveNodePayload {
    selector: String,
    expected_version: u64,
}

#[derive(Deserialize)]
struct CreateNodePayload {
    parent: String,
    title: String,
    #[serde(default)]
    content: Option<String>,
    /// Explicit slug override. Blank/omitted falls back to the same
    /// title-derived, collision-suffixed slug the CLI and MCP `create_node`
    /// use; a supplied slug is instead validated and must not collide with a
    /// sibling's, since silently mangling a value the user deliberately
    /// chose would defeat the point of exposing the field at all.
    #[serde(default)]
    slug: Option<String>,
}

/// Parses one incoming WebSocket text frame and applies it if it is a known
/// command. Returns `None` for anything that is not a command envelope at
/// all (malformed input, or a message type this connection never sends),
/// which the caller silently ignores rather than answering.
pub(crate) fn handle(text: &str, workspace: &WorkspaceState) -> Option<CommandOutcome> {
    let envelope: IncomingEnvelope = serde_json::from_str(text).ok()?;
    if envelope.kind != "command" {
        return None;
    }
    let command = envelope.payload.get("command")?.as_str()?.to_string();
    Some(match command.as_str() {
        "reorder_node" => reorder_node(workspace, envelope.payload),
        "move_subtree" => move_subtree(workspace, envelope.payload),
        "update_node" => update_node(workspace, envelope.payload),
        "create_node" => create_node(workspace, envelope.payload),
        "remove_node" => remove_node(workspace, envelope.payload),
        other => CommandOutcome::reject(other.to_string(), "unknown command".into()),
    })
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn reorder_node(workspace: &WorkspaceState, payload: Value) -> CommandOutcome {
    let params: ReorderNodePayload = match serde_json::from_value(payload) {
        Ok(params) => params,
        Err(error) => return CommandOutcome::reject("reorder_node".into(), error.to_string()),
    };
    let mut store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");

    let selector = match NodeSelector::from_str(&params.selector) {
        Ok(selector) => selector,
        Err(error) => return CommandOutcome::reject("reorder_node".into(), error.to_string()),
    };
    let current = match store.resolve(&selector) {
        Ok(Some(node)) => node,
        Ok(None) => return CommandOutcome::reject("reorder_node".into(), "node not found".into()),
        Err(error) => return CommandOutcome::reject("reorder_node".into(), error.to_string()),
    };
    let Some(parent_id) = current.parent_id() else {
        return CommandOutcome::reject(
            "reorder_node".into(),
            "workspace root cannot be reordered".into(),
        );
    };
    let fields = current.fields();
    if fields.version != params.expected_version {
        return CommandOutcome::reject(
            "reorder_node".into(),
            "version conflict: the node changed since the drag began".into(),
        );
    }

    let sibling_count = match store.children(parent_id) {
        Ok(children) => children.len(),
        Err(error) => return CommandOutcome::reject("reorder_node".into(), error.to_string()),
    };
    // Reorder positions are zero-based canonical indexes. The store removes
    // the target before inserting it, so every in-range index is unambiguous.
    let order = params
        .sibling_order
        .min(u32::try_from(sibling_count.saturating_sub(1)).unwrap_or(u32::MAX));
    let now = now_millis();

    let prepared = match prepare_node_mutation(
        NodeMutationDraft {
            id: current.id(),
            parent_id: Some(parent_id),
            slug: fields.slug.clone(),
            metadata: fields.metadata.clone(),
            markdown_content: fields.markdown_content.clone(),
            sibling_order: order,
            version: fields.version + 1,
            created_at: fields.created_at,
            updated_at: now,
            created_by: None,
            change_summary: Some("Reorder node via browse-ui".into()),
        },
        &SystemUlidGenerator,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return CommandOutcome::reject("reorder_node".into(), error.to_string()),
    };

    let expected_version = fields.version;
    match store.reorder_node(prepared.change(expected_version)) {
        Ok(_) => CommandOutcome::ack("reorder_node".into(), prepared.node.fields().version),
        Err(error) => CommandOutcome::reject("reorder_node".into(), error.to_string()),
    }
}

fn move_subtree(workspace: &WorkspaceState, payload: Value) -> CommandOutcome {
    let params: MoveSubtreePayload = match serde_json::from_value(payload) {
        Ok(params) => params,
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    let mut store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");

    let selector = match NodeSelector::from_str(&params.selector) {
        Ok(selector) => selector,
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    let current = match store.resolve(&selector) {
        Ok(Some(node)) => node,
        Ok(None) => return CommandOutcome::reject("move_subtree".into(), "node not found".into()),
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    if current.is_root() {
        return CommandOutcome::reject(
            "move_subtree".into(),
            "workspace root cannot be moved".into(),
        );
    }
    let fields = current.fields();
    if fields.version != params.expected_version {
        return CommandOutcome::reject(
            "move_subtree".into(),
            "version conflict: the node changed since the drag began".into(),
        );
    }

    let new_parent_selector = match NodeSelector::from_str(&params.new_parent) {
        Ok(selector) => selector,
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    let new_parent_id = match store.resolve(&new_parent_selector) {
        Ok(Some(node)) => node.id(),
        Ok(None) => {
            return CommandOutcome::reject(
                "move_subtree".into(),
                "destination parent not found".into(),
            )
        }
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    if new_parent_id == current.id() {
        return CommandOutcome::reject(
            "move_subtree".into(),
            "cannot move a node below itself".into(),
        );
    }
    let descendants = match store.descendants(current.id()) {
        Ok(items) => items,
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    if descendants
        .iter()
        .any(|item| item.node.id() == new_parent_id)
    {
        return CommandOutcome::reject(
            "move_subtree".into(),
            "cannot move a node below its own subtree".into(),
        );
    }

    let (slug, order) = match store.next_child_placement(new_parent_id, &fields.metadata.title) {
        Ok(placement) => placement,
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };
    let now = now_millis();
    let prepared = match prepare_node_mutation(
        NodeMutationDraft {
            id: current.id(),
            parent_id: Some(new_parent_id),
            slug,
            metadata: fields.metadata.clone(),
            markdown_content: fields.markdown_content.clone(),
            sibling_order: order,
            version: fields.version + 1,
            created_at: fields.created_at,
            updated_at: now,
            created_by: None,
            change_summary: Some("Move subtree via browse-ui".into()),
        },
        &SystemUlidGenerator,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return CommandOutcome::reject("move_subtree".into(), error.to_string()),
    };

    let expected_version = fields.version;
    match store.move_subtree(prepared.change(expected_version)) {
        Ok(_) => CommandOutcome::ack("move_subtree".into(), prepared.node.fields().version),
        Err(error) => CommandOutcome::reject("move_subtree".into(), error.to_string()),
    }
}

fn update_node(workspace: &WorkspaceState, payload: Value) -> CommandOutcome {
    let params: UpdateNodePayload = match serde_json::from_value(payload) {
        Ok(params) => params,
        Err(error) => return CommandOutcome::reject("update_node".into(), error.to_string()),
    };
    let mut store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");

    let selector = match NodeSelector::from_str(&params.selector) {
        Ok(selector) => selector,
        Err(error) => return CommandOutcome::reject("update_node".into(), error.to_string()),
    };
    let current = match store.resolve(&selector) {
        Ok(Some(node)) => node,
        Ok(None) => return CommandOutcome::reject("update_node".into(), "node not found".into()),
        Err(error) => return CommandOutcome::reject("update_node".into(), error.to_string()),
    };
    let fields = current.fields();
    if fields.version != params.expected_version {
        return CommandOutcome::reject(
            "update_node".into(),
            "version conflict: the node changed since editing began".into(),
        );
    }

    let now = now_millis();
    // Title/type/etc. are carried forward unchanged — this command only ever
    // replaces the Markdown body. Renaming is a deliberately separate
    // operation, matching how the MCP `update_node` tool already treats it.
    let prepared = match prepare_node_mutation(
        NodeMutationDraft {
            id: current.id(),
            parent_id: current.parent_id(),
            slug: fields.slug.clone(),
            metadata: fields.metadata.clone(),
            markdown_content: params.content,
            sibling_order: fields.sibling_order,
            version: fields.version + 1,
            created_at: fields.created_at,
            updated_at: now,
            created_by: None,
            change_summary: Some("Edit node content via browse-ui".into()),
        },
        &SystemUlidGenerator,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return CommandOutcome::reject("update_node".into(), error.to_string()),
    };

    let expected_version = fields.version;
    match store.update_node(prepared.change(expected_version)) {
        // A `NoOp` (identical content re-saved) leaves the persisted version
        // unchanged, so the ack must report that unchanged version rather
        // than the speculatively-prepared `expected_version + 1`.
        Ok(mdtree_sqlite::MutationOutcome::Applied) => {
            CommandOutcome::ack("update_node".into(), prepared.node.fields().version)
        }
        Ok(mdtree_sqlite::MutationOutcome::NoOp) => {
            CommandOutcome::ack("update_node".into(), expected_version)
        }
        Err(error) => CommandOutcome::reject("update_node".into(), error.to_string()),
    }
}

fn create_node(workspace: &WorkspaceState, payload: Value) -> CommandOutcome {
    let params: CreateNodePayload = match serde_json::from_value(payload) {
        Ok(params) => params,
        Err(error) => return CommandOutcome::reject("create_node".into(), error.to_string()),
    };
    let mut store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");

    let parent_selector = match NodeSelector::from_str(&params.parent) {
        Ok(selector) => selector,
        Err(error) => return CommandOutcome::reject("create_node".into(), error.to_string()),
    };
    let parent_id = match store.resolve(&parent_selector) {
        Ok(Some(node)) => node.id(),
        Ok(None) => return CommandOutcome::reject("create_node".into(), "parent not found".into()),
        Err(error) => return CommandOutcome::reject("create_node".into(), error.to_string()),
    };
    let siblings = match store.children(parent_id) {
        Ok(children) => children,
        Err(error) => return CommandOutcome::reject("create_node".into(), error.to_string()),
    };
    let order = siblings
        .iter()
        .map(|node| node.fields().sibling_order)
        .max()
        .map_or(0, |order| order.saturating_add(1));
    let requested_slug = params
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let slug = match requested_slug {
        Some(raw) => {
            let slug = match Slug::from_str(raw) {
                Ok(slug) => slug,
                Err(error) => {
                    return CommandOutcome::reject("create_node".into(), error.to_string())
                }
            };
            if siblings.iter().any(|node| node.fields().slug == slug) {
                return CommandOutcome::reject(
                    "create_node".into(),
                    format!("slug \"{slug}\" is already used by a sibling"),
                );
            }
            slug
        }
        None => mdtree_core::generate_slug(
            &params.title,
            siblings.iter().map(|node| &node.fields().slug),
        ),
    };

    let now = now_millis();
    let id = NodeId::new(SystemUlidGenerator.generate());
    let content = params
        .content
        .unwrap_or_else(|| format!("# {}\n", params.title));
    let prepared = match prepare_node_mutation(
        NodeMutationDraft {
            id,
            parent_id: Some(parent_id),
            slug,
            metadata: NodeMetadata::new(&params.title),
            markdown_content: content,
            sibling_order: order,
            version: 1,
            created_at: now,
            updated_at: now,
            created_by: None,
            change_summary: Some("Create node via browse-ui".into()),
        },
        &SystemUlidGenerator,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return CommandOutcome::reject("create_node".into(), error.to_string()),
    };

    match store.create_node(&prepared.node, &prepared.revision, &prepared.derived) {
        Ok(()) => CommandOutcome::created(
            "create_node".into(),
            prepared.node.fields().version,
            prepared.node.id().to_string(),
        ),
        Err(error) => CommandOutcome::reject("create_node".into(), error.to_string()),
    }
}

// The client always confirms with the user before ever sending this (see
// requestDeleteNode in app.js) — the version check below is only about a
// concurrent change since that confirmation was shown, not a substitute for
// it.
fn remove_node(workspace: &WorkspaceState, payload: Value) -> CommandOutcome {
    let params: RemoveNodePayload = match serde_json::from_value(payload) {
        Ok(params) => params,
        Err(error) => return CommandOutcome::reject("remove_node".into(), error.to_string()),
    };
    let mut store = workspace
        .store
        .lock()
        .expect("workspace store mutex poisoned");

    let selector = match NodeSelector::from_str(&params.selector) {
        Ok(selector) => selector,
        Err(error) => return CommandOutcome::reject("remove_node".into(), error.to_string()),
    };
    let current = match store.resolve(&selector) {
        Ok(Some(node)) => node,
        Ok(None) => return CommandOutcome::reject("remove_node".into(), "node not found".into()),
        Err(error) => return CommandOutcome::reject("remove_node".into(), error.to_string()),
    };

    match store.remove_subtree(current.id(), params.expected_version, false) {
        Ok(_) => CommandOutcome::removed("remove_node".into()),
        Err(error) => CommandOutcome::reject("remove_node".into(), error.to_string()),
    }
}
