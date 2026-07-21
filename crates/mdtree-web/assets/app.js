"use strict";

const SVG_NS = "http://www.w3.org/2000/svg";
// Node cards render as fixed-size HTML elements (never resized by content or
// state) positioned in the same "world" coordinate space as the SVG edge
// layer beneath them; x/y always mean a card's *center*, matching the SVG
// convention this replaced.
// Wide enough that the header row's fixed-width chrome (disclosure triangle,
// type icon, reload control, and the "more actions" trigger — every one of
// them reserving its slot even while hidden, so hovering never shifts the
// layout) leaves a genuinely readable amount of room for the title itself,
// rather than truncating most titles to a handful of characters.
const CARD_WIDTH = 240;
// Base card height for a node with no outgoing references — tall enough
// for the icon/title row and the labeled slug field. A node that does have
// references gets `RELATIONS_ROW_HEIGHT` more (see `cardHeightFor`) for its
// own labeled row below the slug, rather than every card reserving that
// space whether or not it actually has anything to show there.
const CARD_HEIGHT = 100;
const RELATIONS_ROW_HEIGHT = 28;
// Fixed vertical gap between one sibling row and the next, on top of
// whichever height that row's own card actually needs.
const SLOT_GAP = 24;
// Card width plus the same 56px inter-generation gap the previous, narrower
// card width also used — only the card grew, not the gap between them.
const LEVEL_WIDTH = CARD_WIDTH + 56;
const CANVAS_MARGIN = 24;
// x position a card by its *center*, so the root's is offset by half a
// card from the margin — otherwise its center would sit at the margin and
// its left half would render off-canvas, off-screen at the default camera.
// This is what actually puts the root card's corner at the top-left of the
// canvas, not just its center near the origin. (The equivalent y offset
// depends on the root's own height, so it's computed where the root is
// placed — see layoutTree — rather than as a fixed constant here.)
const ROW_START_X = CANVAS_MARGIN + CARD_WIDTH / 2;

// How many relation dots fit across one row before wrapping to the next —
// derived from the card's fixed content width and each dot's own size/gap
// (see `.node-card-relation-dot`/`relationsRow` in buildNodeCard) rather than
// hand-picked, since CARD_WIDTH is the same for every card. A node with more
// outgoing references than this wraps instead of running off the card's
// right edge.
const RELATIONS_BLOCK_PADDING_X = 20; // px-2.5 on both sides
const RELATIONS_DOT_SIZE = 8; // h-2 w-2
const RELATIONS_DOT_GAP = 6; // gap-1.5, used for both the row's own gap and (now that dots wrap) the gap between wrapped rows
const RELATIONS_DOTS_PER_ROW = Math.floor(
  (CARD_WIDTH - RELATIONS_BLOCK_PADDING_X + RELATIONS_DOT_GAP) / (RELATIONS_DOT_SIZE + RELATIONS_DOT_GAP),
);
// Each row beyond the first costs one dot's height plus the gap above it;
// the first row's cost is already folded into RELATIONS_ROW_HEIGHT below.
const RELATIONS_EXTRA_ROW_HEIGHT = RELATIONS_DOT_SIZE + RELATIONS_DOT_GAP;

function relationsRowCount(node) {
  const count = node.references?.length ?? 0;
  return count ? Math.ceil(count / RELATIONS_DOTS_PER_ROW) : 0;
}

// A card's actual rendered height: the base height, plus room for the
// labeled relations row(s) when the node has any outgoing references — kept
// per-node (rather than a single fixed constant) so a node without
// references isn't left with blank reserved space at the bottom of an
// otherwise-uniform card, and a node with many wraps to exactly as many rows
// as it needs.
function cardHeightFor(node) {
  const rows = relationsRowCount(node);
  if (!rows) {
    return CARD_HEIGHT;
  }
  return CARD_HEIGHT + RELATIONS_ROW_HEIGHT + (rows - 1) * RELATIONS_EXTRA_ROW_HEIGHT;
}

const RECONNECT_BASE_DELAY_MS = 500;
const RECONNECT_MAX_DELAY_MS = 10000;
// Consecutive failed reconnect attempts tolerated before giving up. A
// transient network drop or a brief server restart resolves well within
// this many bounded-backoff retries (~15s); beyond it, the server process is
// presumed gone rather than momentarily unreachable, and retrying forever
// would never distinguish "still trying" from "never coming back".
const MAX_RECONNECT_ATTEMPTS = 5;

const MIN_ZOOM = 0.2;
const MAX_ZOOM = 2.5;
const ZOOM_MARGIN = 40;

// One tree's worth of mutable UI state: node cache, expand/selection state,
// camera, and in-flight command bookkeeping. Each open workspace gets its
// own; `state` (below) always points at the active one.
function createWorkspaceState() {
  return {
    root: null,
    nodes: new Map(),
    // Reverse lookup from a node id to its parent's id, populated as node data
    // loads, so keyboard navigation and active-path highlighting don't need a
    // separate request to find an ancestor.
    parents: new Map(),
    expanded: new Set(),
    selected: null,
    // Id of the node whose Markdown is currently shown in the viewer, or null
    // when it is closed. Distinct from `selected`: moving the tree selection
    // with arrow keys does not change what the open viewer displays.
    viewerNodeId: null,
    // Nodes presumed stale after a reconnect gap or a live change notification
    // that can't be attributed to a specific node, OR explicitly flagged by
    // our own create/update/reorder/move as a "this just changed" signal on
    // the one node it actually affected (see e.g. handleCreateNodeResponse).
    // A stale node is muted and disabled except for its reload action.
    stale: new Set(),
    // Nodes currently being reloaded; shown as a distinct, non-interactive
    // state without resizing the node.
    loading: new Set(),
    // Pan/zoom is purely tab-local presentation state: never broadcast,
    // persisted, or reset by re-rendering (expand/collapse, selection, the
    // Markdown viewer opening or closing).
    camera: { x: 0, y: 0, scale: 1 },
    // Bounding box of the last rendered layout, in unscaled tree coordinates;
    // used by fit-to-view (F).
    bounds: null,
    // Last-rendered tree-coordinate position of every visible node, used by
    // drag-to-reorder to translate a pointer position into a sibling index.
    positions: new Map(),
    // Set once a reorder/reparent command has been sent and cleared on its
    // ack or reject; the layout settles only after the server responds, and
    // only one structural command is outstanding at a time.
    dragPending: null,
    // Set while the reading pane shows the Markdown-body editor for
    // `selected`, cleared on Cancel or a successful Save — distinct from
    // `pendingEdit` (below), which is only set while a Save is actually in
    // flight.
    editing: null,
    // Set while a `update_node` command sent from the editor is awaiting its
    // ack/reject, kept separate from `dragPending` so a drag and a Save can
    // never clobber each other's pending-command bookkeeping.
    pendingEdit: null,
    // Set while the reading pane shows the "new child node" form, cleared on
    // Cancel or a successful Create.
    creating: null,
    // Set while a `create_node` command is awaiting its ack/reject.
    pendingCreate: null,
    // Set once a `remove_node` command has been sent (from the "More
    // actions" menu's Delete, after the user confirms) and cleared on its
    // ack or reject.
    pendingDelete: null,
    // True while a staged expand-all traversal (Alt+E, Shift+E, or the
    // Expand menu) is in flight, so the status bar can surface that it is
    // running and cancellable.
    expanding: false,
    // Relation-type -> color-class assignments discovered in this
    // workspace's loaded nodes so far (see `colorForRelation`).
    relationColors: new Map(),
    // Id of the node whose subtree is exclusively shown ("focus mode"), or
    // null in the normal view. Everything outside it renders blurred and
    // inert rather than being removed from the layout — see render().
    focusedNodeId: null,
  };
}

// One entry per workspace open in this session, keyed by the server-assigned
// id (also its position in the switcher panel and its `/api/{id}/...` URL
// segment). A workspace's own socket/reconnect/revision bookkeeping lives
// here (not on `state`) so a backgrounded workspace's live updates land in
// the right place even while a different workspace is on screen.
const workspaces = new Map();
let activeWorkspaceId = null;
// Process-wide credential (same for every workspace in this session),
// fetched once from `/api/workspaces`.
let sessionCredential = null;

// The active workspace's mutable tree/camera/selection state. Reassigned by
// `switchWorkspace()`; the large majority of this file reads/writes this
// module-level binding directly, so switching workspaces is mostly just
// swapping what it points at and re-rendering.
let state = createWorkspaceState();

// The one live EasyMDE instance backing whichever of `#viewer-editor-textarea`
// / `#viewer-create-textarea` is currently shown — a transient UI widget, not
// per-workspace data, so it lives here rather than on `state`. Never more
// than one is mounted at a time (entering create mode exits edit mode and
// vice versa).
let easyMdeInstance = null;

async function fetchJson(path) {
  const response = await fetch(path);
  if (!response.ok) {
    throw new Error(`request failed: ${path} (${response.status})`);
  }
  return response.json();
}

// Fetches and caches one node into `workspaceId`'s own state — explicit
// about which workspace it targets (rather than the reassignable `state`
// binding) so a switch mid-fetch can never land data in the wrong tree.
async function loadNode(workspaceId, id) {
  const node = await fetchJson(`/api/${workspaceId}/node/${encodeURIComponent(id)}`);
  const workspaceState = workspaces.get(workspaceId).state;
  workspaceState.nodes.set(node.id, node);
  trackRelations(workspaceState, node);
  for (const child of node.children) {
    workspaceState.parents.set(child.id, node.id);
    trackRelations(workspaceState, child);
  }
  return node;
}

// Relation types are free-text (not a fixed enum — see the spec's typed
// references), so most colors are assigned the first time each is seen and
// stay stable for the rest of the session, cycling through the palette if
// there happen to be more distinct types than colors. A few conventional
// names get a fixed, meaningful color instead of whatever's next in line.
const RELATION_COLOR_PALETTE = [
  "bg-amber-400",
  "bg-sky-400",
  "bg-fuchsia-400",
  "bg-lime-400",
  "bg-orange-400",
  "bg-cyan-400",
  "bg-pink-400",
  "bg-teal-400",
];
const FIXED_RELATION_COLORS = {
  done: "bg-green-500",
  error: "bg-red-500",
  "in-progress": "bg-blue-500",
  "in progress": "bg-blue-500",
};

function colorForRelation(workspaceState, type) {
  const relationColors = workspaceState.relationColors;
  if (!relationColors.has(type)) {
    const fixed = FIXED_RELATION_COLORS[type.toLowerCase()];
    relationColors.set(
      type,
      fixed ?? RELATION_COLOR_PALETTE[relationColors.size % RELATION_COLOR_PALETTE.length],
    );
  }
  return relationColors.get(type);
}

function capitalize(text) {
  return text.length === 0 ? text : text[0].toUpperCase() + text.slice(1);
}

function trackRelations(workspaceState, summary) {
  for (const reference of summary.references ?? []) {
    colorForRelation(workspaceState, reference.reference_type);
  }
}

// The bottom-left legend reflects every relation type discovered so far
// among loaded nodes — there is no single "list every relation type in the
// workspace" endpoint, so a not-yet-loaded node's relation kind (if novel)
// only appears here once that node has actually been fetched.
// Built from the exact same classes as a workspace item (see
// setUpWorkspacePanel) — same row/avatar/name structure, so it reads as a
// continuation of that same list rather than a visually distinct block, and
// so it automatically picks up the same collapsed-to-icons treatment
// (setWorkspacePanelCollapsed toggles `.workspace-panel-item-name` and
// `.workspace-panel-item-row` across the whole list, not just the
// workspace items themselves).
function renderRelationsLegend() {
  const legend = document.getElementById("relations-legend");
  const label = document.getElementById("relations-legend-label");
  legend.textContent = "";
  const collapsed = document.getElementById("workspace-list").classList.contains("collapsed");
  const hasRelations = state.relationColors.size > 0;
  legend.hidden = !hasRelations;
  label.hidden = !hasRelations || collapsed;
  if (!hasRelations) {
    return;
  }
  for (const [type, colorClass] of state.relationColors) {
    const item = document.createElement("div");
    item.className = "workspace-panel-item";
    const row = document.createElement("div");
    // Rebuilt from scratch on every render (unlike the workspace buttons,
    // which are created once in setUpWorkspacePanel and just get toggled in
    // place) — the collapsed/centered state has to be set here directly
    // every time rather than relying on setWorkspacePanelCollapsed's own
    // pass over `.workspace-panel-item-row`, which only ever touches
    // whatever rows already existed at the moment it ran.
    row.className = `workspace-panel-item-row flex items-center gap-2${collapsed ? " justify-center" : ""}`;
    const swatch = document.createElement("span");
    swatch.className = `h-7 w-7 flex-none ${colorClass}`;
    const name = document.createElement("span");
    name.className = "workspace-panel-item-name min-w-0 flex-1 truncate";
    name.hidden = collapsed;
    name.textContent = capitalize(type);
    row.append(swatch, name);
    item.appendChild(row);
    legend.appendChild(item);
  }
}

async function init() {
  const session = await fetchJson("/api/workspaces");
  sessionCredential = session.session_credential;
  document.getElementById("app-version").textContent = `v${session.server_version}`;
  for (const summary of session.workspaces) {
    const workspaceState = createWorkspaceState();
    // Assigns every relation type its legend color up front, from the
    // server's workspace-wide scan — otherwise a type only earns a color
    // (and a legend entry) once some node using it happens to be loaded,
    // which could be well after the workspace first opens.
    for (const type of summary.relation_types) {
      colorForRelation(workspaceState, type);
    }
    workspaces.set(summary.id, {
      id: summary.id,
      name: summary.name,
      root: summary.root,
      state: workspaceState,
      socket: null,
      reconnectAttempt: 0,
      intentionalShutdown: false,
      lastKnownRevision: null,
      // Incremented whenever our own create/update/reorder/move ack already
      // flagged precisely the one node it affected (see e.g.
      // handleCreateNodeResponse) — decremented by the next "change"
      // broadcast that same mutation triggers (always sent after our ack,
      // over this same connection — see change_hub.rs), which then skips
      // the usual "can't tell what changed, mark everything loaded stale"
      // fallback entirely for that one event, since we already know exactly
      // what changed and already flagged it ourselves.
      suppressNextChangeSweeps: 0,
      loaded: false,
    });
  }
  setUpPanAndZoom();
  setUpStatusControls();
  setUpWorkspacePanel();
  setUpSearch();
  document.getElementById("reading-pane").hidden = localStorage.getItem(READING_PANE_HIDDEN_KEY) === "true";
  setUpReadingPaneResize();
  await switchWorkspace(session.workspaces[0].id);
}

function isConnected() {
  const socket = workspaces.get(activeWorkspaceId)?.socket;
  return Boolean(socket) && socket.readyState === WebSocket.OPEN;
}

function setConnectionState(connectionState) {
  const element = document.getElementById("connection-state");
  element.dataset.state = connectionState;
  // The state is surfaced only as a hover tooltip (no visible text label),
  // so the dot alone stays a compact, unobtrusive header element.
  element.title = capitalize(connectionState);
}

// Shared by the Expand menu's "root node" option (setUpStatusControls) and
// the Alt+E shortcut. Selects the root once everything is open (setSelected
// covers the render its own state.selected change needs), then fits the
// camera anchored top-left (not centered), so the root — and where to
// start reading — is always exactly where the eye expects it, the same
// corner it started in before expanding.
function expandRoot() {
  return expandSubtree(state.root)
    .then(() => {
      setSelected(state.root);
      fitToTopLeft();
    })
    .catch(reportError);
}

// Shared by the Expand menu's "current node" option and the Shift+E
// shortcut. A no-op with nothing selected, since there'd be no subtree to
// expand.
function expandSelected() {
  if (state.selected) {
    return expandSubtree(state.selected).then(() => fitToTopLeft()).catch(reportError);
  }
  return undefined;
}

// Shared by the Collapse menu's "root node" option and the Alt+C shortcut.
function collapseRoot() {
  collapseSubtree(state.root);
  // Collapsing everything can hide the very branch focus mode was isolating
  // (or leave "show only this branch" pointing at a node that no longer
  // reads as a meaningful subtree to isolate) — exit it rather than leaving
  // a stale focus banner/blur active.
  state.focusedNodeId = null;
  // Selects the now-only-remaining root card (setSelected's own render()
  // picks up both that and the cleared focus-mode state above).
  setSelected(state.root);
  // With everything collapsed, only the root card remains — recenter the
  // camera on it rather than leaving the view wherever it happened to be
  // panned/zoomed to beforehand. A plain fit-to-view would zoom a single
  // small card up to MAX_ZOOM to fill the canvas, making it look huge;
  // capping it at the default scale keeps the root a normal size, anchored
  // top-left like every other expand/collapse action here.
  fitToTopLeft(1);
}

// Shared by the Collapse menu's "current node" option and the Shift+C
// shortcut. A no-op with nothing selected.
function collapseSelected() {
  if (state.selected) {
    collapseSubtree(state.selected);
    render();
    fitToTopLeft(1);
  }
}

function setUpStatusControls() {
  setUpRootCurrentMenu("control-fit", "fit-menu", "fit-menu-root", "fit-menu-selected", {
    // A collapsed root (nothing else visible) fits into a much larger scale
    // than an expanded tree normally does — same reasoning as
    // control-close-all's own `fitToTopLeft(1)` — so cap it the same way
    // rather than blowing a single card up to fill the canvas.
    onRoot: () => fitToTopLeft(state.expanded.has(state.root) ? undefined : 1),
    onSelected: () => fitToView(),
  });
  setUpRootCurrentMenu("control-open-all", "expand-menu", "expand-menu-root", "expand-menu-selected", {
    onRoot: expandRoot,
    onSelected: expandSelected,
    isRootRedundant: () => state.expanded.has(state.root),
    isSelectedRedundant: () => state.expanded.has(state.selected),
  });
  setUpRootCurrentMenu("control-close-all", "collapse-menu", "collapse-menu-root", "collapse-menu-selected", {
    onRoot: collapseRoot,
    onSelected: collapseSelected,
    isRootRedundant: () => !state.expanded.has(state.root),
    isSelectedRedundant: () => !state.expanded.has(state.selected),
  });
  document.getElementById("control-zoom-in").addEventListener("click", () => {
    zoomIn();
  });
  document.getElementById("control-zoom-out").addEventListener("click", () => {
    zoomOut();
  });
  document.getElementById("control-theme").addEventListener("click", () => {
    toggleTheme();
  });
  document.getElementById("control-help").addEventListener("click", () => {
    toggleShortcutHelp();
  });
  document.getElementById("control-stop").addEventListener("click", () => {
    stopServer().catch(reportError);
  });
}

// The `dark` class is applied to <html> (rather than relying on
// prefers-color-scheme) so the user's explicit choice always wins, and an
// inline snippet in index.html mirrors this before first paint to avoid a
// light-theme flash for users who picked dark.
function toggleTheme() {
  const isDark = document.documentElement.classList.toggle("dark");
  localStorage.setItem("mdtree-theme", isDark ? "dark" : "light");
}

// Authenticated Stop: any client holding the per-launch session credential
// may end the session for everyone. A confirmation guards against an
// accidental click, since this affects every connected tab/process, not
// just the one clicking it.
async function stopServer() {
  if (!window.confirm("Stop the browse-ui server for all connected clients?")) {
    return;
  }
  await fetch("/api/stop", {
    method: "POST",
    headers: { "x-mdtree-session": sessionCredential },
  });
}

// Each open workspace keeps its own persistent connection (rather than one
// shared/multiplexed socket) so a backgrounded workspace keeps receiving
// live change notifications and switching back to it never needs a
// reconnect. `workspace`'s own reconnect/revision bookkeeping — not the
// reassignable `state`/active-workspace globals — is what this closes over,
// so events for a backgrounded workspace always land in the right place.
function connectWebSocket(workspace) {
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  // The WebSocket constructor cannot set custom request headers, so the
  // session credential — required here the same as for Stop, since this
  // channel grants live structural-mutation authority — travels as a query
  // parameter instead.
  const session = encodeURIComponent(sessionCredential);
  const socket = new WebSocket(
    `${protocol}//${window.location.host}/api/ws/${workspace.id}?session=${session}`,
  );
  workspace.socket = socket;
  if (workspace.id === activeWorkspaceId) {
    window.__mdtreeSocket = socket;
  }

  socket.addEventListener("open", () => {
    workspace.reconnectAttempt = 0;
    if (workspace.id === activeWorkspaceId) {
      setConnectionState("connected");
    }
  });
  socket.addEventListener("close", () => {
    if (workspace.socket === socket) {
      workspace.socket = null;
    }
    if (workspace.intentionalShutdown) {
      // The server told us it is stopping; settle into a closed-session
      // state distinct from a transient disconnect rather than retrying.
      if (workspace.id === activeWorkspaceId) {
        setConnectionState("closed");
      }
      return;
    }
    if (workspace.reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      // Killing the server process (rather than a graceful Stop, and rather
      // than a transient network blip) looks identical from here: the
      // socket just closes. Past this many consecutive failures, presume
      // it is gone and settle into a distinct terminal state instead of
      // retrying forever — the canvas and document snapshot stay exactly
      // as they were.
      if (workspace.id === activeWorkspaceId) {
        setConnectionState("lost");
      }
      return;
    }
    if (workspace.id === activeWorkspaceId) {
      setConnectionState("disconnected");
    }
    scheduleReconnect(workspace);
  });
  socket.addEventListener("message", (event) => {
    let envelope;
    try {
      envelope = JSON.parse(event.data);
    } catch (error) {
      reportError(error);
      return;
    }
    if (envelope.v !== 1) {
      // Unknown protocol version: fail closed rather than guess at its shape.
      socket.close();
      return;
    }
    if (envelope.type === "shutdown") {
      workspace.intentionalShutdown = true;
    } else if (envelope.type === "init") {
      // A fresh `init` (i.e. this is a reconnect, not the first-ever load)
      // whose revision differs from the last one we knew about means the
      // workspace changed while this tab had no live channel to be told
      // about it — mark loaded data stale rather than silently keep
      // presenting it as current.
      if (
        workspace.lastKnownRevision !== null &&
        envelope.revision !== workspace.lastKnownRevision
      ) {
        markLoadedNodesStale(workspace);
      }
      workspace.lastKnownRevision = envelope.revision ?? workspace.lastKnownRevision;
    } else if (envelope.type === "change") {
      workspace.lastKnownRevision = envelope.revision ?? workspace.lastKnownRevision;
      if (workspace.suppressNextChangeSweeps > 0) {
        workspace.suppressNextChangeSweeps -= 1;
      } else {
        markLoadedNodesStale(workspace);
      }
    } else if (envelope.type === "ack" || envelope.type === "reject") {
      // Acks/rejects only ever answer a command the active workspace sent
      // (drag-to-reorder/reparent is only ever performed on the visible
      // tree), so a response arriving for a now-backgrounded workspace is
      // simply not applied.
      if (workspace.id === activeWorkspaceId) {
        handleCommandResponse(envelope).catch(reportError);
      }
    }
    // "heartbeat" carries no further client-visible action.
  });
}

function sendCommand(command, payload) {
  const workspace = workspaces.get(activeWorkspaceId);
  if (!workspace || !isConnected()) {
    return false;
  }
  workspace.socket.send(
    JSON.stringify({
      v: 1,
      id: `${Date.now()}-${Math.random().toString(36).slice(2)}`,
      session: "",
      type: "command",
      payload: { command, ...payload },
    }),
  );
  return true;
}

function scheduleReconnect(workspace) {
  const delay = Math.min(
    RECONNECT_BASE_DELAY_MS * 2 ** workspace.reconnectAttempt,
    RECONNECT_MAX_DELAY_MS,
  );
  workspace.reconnectAttempt += 1;
  setTimeout(() => {
    if (!workspace.intentionalShutdown) {
      connectWebSocket(workspace);
    }
  }, delay);
}

function markLoadedNodesStale(workspace) {
  // The workspace revision counter is workspace-wide, not per-node, so a
  // change (whether from a live "change" event or a reconnect gap) can't be
  // attributed to a specific node. Conservatively mark everything currently
  // loaded stale rather than silently refreshing or guessing.
  const workspaceState = workspace.state;
  for (const id of workspaceState.nodes.keys()) {
    workspaceState.stale.add(id);
  }
  // The reading pane can display a node's rendered Markdown without ever
  // caching its full node-with-children data in state.nodes (updateReadingPane
  // fetches /render directly), so it needs to be covered here too.
  if (workspaceState.viewerNodeId) {
    workspaceState.stale.add(workspaceState.viewerNodeId);
  }
  if (workspace.id === activeWorkspaceId) {
    render();
  }
}

// Builds the left-hand workspace switcher panel once, from the workspace
// list fetched in `init()`. Hidden entirely when only one workspace is
// open, so the default single-workspace case looks exactly as it did before
// multi-workspace support existed.
// Distinct per-workspace avatar colors, cycling through the palette in
// listed (already-alphabetical, per the server) order — the same pattern as
// `colorForRelation`, just keyed by position instead of first-seen text.
const WORKSPACE_AVATAR_PALETTE = [
  "bg-indigo-500",
  "bg-emerald-500",
  "bg-amber-500",
  "bg-sky-500",
  "bg-rose-500",
  "bg-fuchsia-500",
  "bg-teal-500",
  "bg-orange-500",
];
const WORKSPACE_PANEL_COLLAPSED_KEY = "mdtree-workspace-panel-collapsed";
const COLLAPSE_ICON_PATH = "M13 4l-6 6 6 6";
const EXPAND_ICON_PATH = "M7 4l6 6-6 6";

// Always shown (even for a single workspace) — the panel is a permanent
// fixture of the left rail now, not a switcher that only earns its keep
// once a second workspace is open.
function setUpWorkspacePanel() {
  const panel = document.getElementById("workspace-panel");

  document.getElementById("workspace-panel-toggle").addEventListener("click", () => {
    setWorkspacePanelCollapsed(!document.getElementById("workspace-list").classList.contains("collapsed"));
  });

  let index = 0;
  for (const workspace of workspaces.values()) {
    const button = document.createElement("button");
    button.type = "button";
    button.dataset.workspaceId = String(workspace.id);
    // The hover highlight lives here (not on the shared `.workspace-panel-item`
    // class) because it's specific to this being a clickable switcher — the
    // relations legend below shares the same base class for a unified look
    // but isn't interactive, so it shouldn't highlight on hover.
    button.className =
      "workspace-panel-item hover:bg-slate-200/60 hover:text-slate-900 dark:hover:bg-slate-800 dark:hover:text-slate-100";
    button.setAttribute("aria-label", workspace.name);

    // A colored initial icon chip (icon-only when the panel is collapsed,
    // with a custom popup tooltip standing in for the hidden name — see
    // showTooltip) plus the full name, rather than plain text — with several
    // workspaces open, the color gives a quicker at-a-glance distinction
    // than the name alone.
    const row = document.createElement("div");
    row.className = "workspace-panel-item-row flex items-center gap-2";
    const avatar = document.createElement("span");
    avatar.className = `flex h-7 w-7 flex-none items-center justify-center text-xs font-semibold text-white ${WORKSPACE_AVATAR_PALETTE[index % WORKSPACE_AVATAR_PALETTE.length]}`;
    avatar.textContent = (workspace.name.trim()[0] ?? "?").toUpperCase();
    const name = document.createElement("span");
    name.className = "workspace-panel-item-name min-w-0 flex-1 truncate";
    name.textContent = workspace.name;
    row.append(avatar, name);
    button.appendChild(row);

    button.addEventListener("click", () => {
      hideTooltip();
      switchWorkspace(workspace.id).catch(reportError);
    });
    // Only useful once the panel is collapsed to icons — the expanded panel
    // already shows the name as text.
    button.addEventListener("pointerenter", () => {
      if (document.getElementById("workspace-list").classList.contains("collapsed")) {
        showTooltip(button, workspace.name);
      }
    });
    button.addEventListener("pointerleave", hideTooltip);
    panel.appendChild(button);
    index += 1;
  }
  setWorkspacePanelCollapsed(localStorage.getItem(WORKSPACE_PANEL_COLLAPSED_KEY) === "true");
  renderWorkspacePanel();
}

// Shrinks the workspace list to just its icon chips (or restores the
// full-width list with names), persisting the choice the same way the theme
// toggle does, so it survives a page reload. The icon-rail column next to it
// is unaffected — it's icon-only regardless.
function setWorkspacePanelCollapsed(collapsed) {
  const list = document.getElementById("workspace-list");
  const label = document.getElementById("workspace-list-label");
  list.classList.toggle("collapsed", collapsed);
  list.classList.toggle("w-44", !collapsed);
  list.classList.toggle("w-14", collapsed);
  label.hidden = collapsed;
  // The brand row's name/version only make sense at the panel's full width;
  // collapsing hides them too (rather than just truncating) so the row — and
  // the whole panel — actually shrinks to the icon rail's width, not just
  // the workspace list next to it.
  document.getElementById("brand-name").hidden = collapsed;
  document.getElementById("app-version").hidden = collapsed;
  // Scoped to the whole list (not just `panel`) so this also covers the
  // relations legend's rows, which reuse these same classes — see
  // renderRelationsLegend.
  for (const name of list.querySelectorAll(".workspace-panel-item-name")) {
    name.hidden = collapsed;
  }
  for (const row of list.querySelectorAll(".workspace-panel-item-row")) {
    row.classList.toggle("justify-center", collapsed);
  }
  if (!collapsed) {
    hideTooltip();
  }
  renderRelationsLegend();
  const toggle = document.getElementById("workspace-panel-toggle");
  toggle.title = collapsed ? "Expand workspace panel" : "Collapse to icons only";
  toggle.setAttribute("aria-label", toggle.title);
  const iconPath = collapsed ? EXPAND_ICON_PATH : COLLAPSE_ICON_PATH;
  toggle.innerHTML = `<svg viewBox="0 0 20 20" fill="none" class="h-4 w-4" stroke="currentColor" stroke-width="1.6"><path d="${iconPath}" stroke-linecap="round" stroke-linejoin="round" /></svg>`;
  localStorage.setItem(WORKSPACE_PANEL_COLLAPSED_KEY, collapsed ? "true" : "false");
}

// A custom popup tooltip (rather than the native title attribute, which is
// slow to appear and can't be styled) shared by every hoverable element that
// needs one — the collapsed workspace panel's icon chips, and a node card's
// reference dots. `placement` picks which side of `anchor` the tooltip
// appears on: "right" (vertically centered, e.g. a workspace icon chip) or
// "above" (horizontally centered, e.g. a small inline dot).
function showTooltip(anchor, text, placement = "right") {
  const tooltip = document.getElementById("hover-tooltip");
  const rect = anchor.getBoundingClientRect();
  tooltip.textContent = text;
  if (placement === "above") {
    tooltip.style.left = `${rect.left + rect.width / 2}px`;
    tooltip.style.top = `${rect.top - 8}px`;
    tooltip.style.transform = "translate(-50%, -100%)";
  } else {
    tooltip.style.left = `${rect.right + 8}px`;
    tooltip.style.top = `${rect.top + rect.height / 2}px`;
    tooltip.style.transform = "translateY(-50%)";
  }
  tooltip.hidden = false;
}

function hideTooltip() {
  document.getElementById("hover-tooltip").hidden = true;
}

function renderWorkspacePanel() {
  const panel = document.getElementById("workspace-panel");
  for (const button of panel.querySelectorAll("[data-workspace-id]")) {
    const isActive = Number(button.dataset.workspaceId) === activeWorkspaceId;
    button.classList.toggle("active", isActive);
    button.setAttribute("aria-current", isActive ? "true" : "false");
  }
}

// Switches which open workspace is displayed. The first switch to a given
// workspace fetches its root and opens its own persistent WebSocket
// connection (subsequent switches are instant, reusing already-loaded state
// and a connection kept alive in the background). Guards every mutation
// through `workspace.state` explicitly (never the bare reassignable `state`
// binding) so a rapid second switch mid-fetch can never land data in the
// wrong tree.
async function switchWorkspace(id) {
  const workspace = workspaces.get(id);
  if (!workspace || id === activeWorkspaceId) {
    return;
  }
  cancelActiveGesture();
  // The editor/create form is tied to the outgoing workspace's node — never
  // meaningful once `state` below repoints somewhere else entirely.
  exitEditMode();
  closeCreateChildForm();
  activeWorkspaceId = id;
  state = workspace.state;
  renderWorkspacePanel();
  setConnectionState(isConnected() ? "connected" : "connecting");
  if (!workspace.loaded) {
    workspace.loaded = true;
    workspace.state.root = workspace.root;
    await loadNode(id, workspace.root);
    workspace.state.expanded.add(workspace.root);
    workspace.state.selected = workspace.root;
  }
  if (activeWorkspaceId === id) {
    render();
    // The reading pane otherwise keeps showing whatever the previous
    // workspace's selection last rendered — refresh it for this
    // workspace's own selection instead.
    updateReadingPane(state.selected).catch(reportError);
  }
  if (!workspace.socket) {
    connectWebSocket(workspace);
  }
}

// Header search: a Molport-style typeahead over node titles/slugs, with
// `workspace:`/`slug:`/`text:` qualifiers layered on top of the plain
// substring default (see `/api/search` on the server). Results can point
// into any open workspace; picking one switches to it, expands its
// ancestors (loading any not yet cached), and centers the tree on it.
const SEARCH_DEBOUNCE_MS = 150;
// Bumped on every request sent; a response is only rendered if it is still
// the most recent one, so a slow response to an early keystroke can never
// clobber the result of a faster response to a later one.
let searchRequestToken = 0;
let searchDebounceTimer = null;
// Index of the result ArrowUp/ArrowDown/Enter act on, kept as plain state
// rather than actual DOM focus: focus always stays on the input itself (so
// typing a query is never interrupted), while this drives a visual
// highlight on one `.search-result-item` at a time — the same "roving
// highlight, real focus stays on the input" pattern a combobox uses.
let highlightedResultIndex = 0;

// The search popover is triggered from its own rail button (rather than
// always occupying panel space) and floats over the canvas — `position:
// fixed`, positioned from the trigger button's own rect — so it can be
// noticeably wider than the left panel without being clipped by the
// panel's `overflow-hidden` or squeezed to the panel's width.
function setUpSearch() {
  const input = document.getElementById("search-input");
  const popover = document.getElementById("search-popover");
  const trigger = document.getElementById("control-search");

  trigger.addEventListener("click", () => {
    if (popover.hidden) {
      showSearchPopover();
    } else {
      hideSearchPopover();
    }
  });
  input.addEventListener("input", () => {
    const value = input.value;
    if (searchDebounceTimer !== null) {
      clearTimeout(searchDebounceTimer);
    }
    if (value.trim() === "") {
      searchRequestToken += 1; // invalidate any in-flight request
      showSearchHint();
      return;
    }
    searchDebounceTimer = setTimeout(() => {
      runSearch(value).catch(reportError);
    }, SEARCH_DEBOUNCE_MS);
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      hideSearchPopover();
    } else if (event.key === "Enter") {
      getSearchResultItems()[highlightedResultIndex]?.click();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      moveSearchHighlight(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      moveSearchHighlight(-1);
    }
  });
  // A plain document-level listener (rather than a `blur` on the input) so
  // clicking a result button — which itself steals focus — does not close
  // the popover before its own click handler runs, and so clicking the
  // trigger button again is a plain toggle rather than a close-then-reopen.
  document.addEventListener("pointerdown", (event) => {
    if (
      !popover.hidden &&
      !popover.contains(event.target) &&
      !trigger.contains(event.target)
    ) {
      hideSearchPopover();
    }
  });
}

// Every "root vs current node" mini menu (Fit, Expand, Collapse) shares this
// exact shape: a rail button that toggles a small icon-only popover with two
// options, anchored to its right the same way the search popover is (see
// showSearchPopover) — an explicit choice between acting on the whole tree
// (root) and just the selected node's subtree, rather than the button
// silently guessing which one the user wants. The icon-only buttons carry
// no visible text, so hover wires the shared custom tooltip (rather than the
// native `title`) instead. Every menu opened this way is tracked in
// `openRootCurrentMenus` so a single Escape press (or clicking anywhere
// outside) closes whichever one is open.
//
// The menu only pops up at all when some *other*, non-root node is
// selected — "root" and "current node" would otherwise be the exact same
// action (the root's own subtree already *is* the whole tree), so with
// nothing selected, or the root itself selected, a click just runs the
// root action directly instead of offering a choice with no real
// difference. The trigger itself goes disabled once every action behind it
// would be redundant (see refreshRootCurrentMenuTriggers, called from
// render()).
const openRootCurrentMenus = [];

function setUpRootCurrentMenu(
  triggerId,
  menuId,
  rootButtonId,
  selectedButtonId,
  { onRoot, onSelected, isRootRedundant, isSelectedRedundant },
) {
  const trigger = document.getElementById(triggerId);
  const menu = document.getElementById(menuId);
  const rootButton = document.getElementById(rootButtonId);
  const selectedButton = document.getElementById(selectedButtonId);

  function show() {
    const rect = trigger.getBoundingClientRect();
    menu.style.left = `${rect.right + 8}px`;
    menu.style.top = `${rect.top}px`;
    menu.hidden = false;
    // Expand/collapse pass `isRootRedundant`/`isSelectedRedundant` so each
    // option is disabled once its own target is already in the state it
    // would produce (root already expanded/collapsed, or the selection
    // already is) — Fit passes neither, so both stay enabled (re-fitting is
    // always meaningful).
    rootButton.disabled = Boolean(isRootRedundant && isRootRedundant());
    selectedButton.disabled = Boolean(isSelectedRedundant && isSelectedRedundant());
  }
  function hide() {
    menu.hidden = true;
    hideTooltip();
  }

  trigger.addEventListener("click", () => {
    if (!state.selected || state.selected === state.root) {
      hide();
      // Expand/collapse pass `isRootRedundant` so a click is a no-op once
      // the root is already in the state that action would produce (e.g.
      // Expand when the root is already open) — running it again would
      // just redo the same traversal for nothing. Fit has no such
      // redundant state (re-fitting is always meaningful), so it omits
      // this and always runs.
      if (!isRootRedundant || !isRootRedundant()) {
        onRoot();
      }
      return;
    }
    if (menu.hidden) {
      show();
    } else {
      hide();
    }
  });
  rootButton.addEventListener("pointerenter", () => showTooltip(rootButton, "Root node", "above"));
  rootButton.addEventListener("pointerleave", hideTooltip);
  selectedButton.addEventListener("pointerenter", () => showTooltip(selectedButton, "Current node", "above"));
  selectedButton.addEventListener("pointerleave", hideTooltip);
  rootButton.addEventListener("click", () => {
    hide();
    onRoot();
  });
  selectedButton.addEventListener("click", () => {
    hide();
    onSelected();
  });
  // Same "click outside closes it" pattern as the search popover (see
  // setUpSearch) — a plain document-level listener rather than a `blur`, so
  // clicking the trigger again toggles closed instead of closing then
  // immediately reopening.
  document.addEventListener("pointerdown", (event) => {
    if (!menu.hidden && !menu.contains(event.target) && !trigger.contains(event.target)) {
      hide();
    }
  });
  openRootCurrentMenus.push({ trigger, menu, hide, isRootRedundant, isSelectedRedundant });
}

function hideOpenRootCurrentMenu() {
  for (const { menu, hide } of openRootCurrentMenus) {
    if (!menu.hidden) {
      hide();
    }
  }
}

// A trigger goes inert once *every* action it could take would be a no-op:
// with a distinct node selected, that means both root and current are
// redundant (Fit's own trigger, which passes neither check, is never
// disabled — re-fitting is always meaningful); with nothing distinct
// selected, only the root check applies, since "current node" isn't even
// reachable without a menu to pick it from. Called from render() so it
// stays in sync with selection/expand-state changes made anywhere else.
function refreshRootCurrentMenuTriggers() {
  const hasDistinctSelection = Boolean(state.selected) && state.selected !== state.root;
  for (const { trigger, isRootRedundant, isSelectedRedundant } of openRootCurrentMenus) {
    const rootRedundant = Boolean(isRootRedundant && isRootRedundant());
    trigger.disabled = hasDistinctSelection
      ? rootRedundant && Boolean(isSelectedRedundant && isSelectedRedundant())
      : rootRedundant;
  }
}

// Opens the popover anchored to the right of its trigger button, clears any
// previous query, and shows the qualifier hint — a fresh search every time,
// rather than reopening onto a stale query or stale results.
function showSearchPopover() {
  const popover = document.getElementById("search-popover");
  const trigger = document.getElementById("control-search");
  const rect = trigger.getBoundingClientRect();
  popover.style.left = `${rect.right + 8}px`;
  popover.style.top = `${rect.top}px`;
  popover.hidden = false;
  const input = document.getElementById("search-input");
  input.value = "";
  input.focus();
  showSearchHint();
}

function hideSearchPopover() {
  document.getElementById("search-popover").hidden = true;
}

function showSearchHint() {
  const dropdown = document.getElementById("search-dropdown");
  dropdown.textContent = "";
  const heading = document.createElement("div");
  heading.className =
    "px-3 pb-1 pt-2 text-[10px] font-semibold uppercase tracking-wide text-slate-400 dark:text-slate-500";
  heading.textContent = "Search examples";
  dropdown.appendChild(heading);
  for (const [keyword, placeholder] of [
    ["workspace:", "<value>"],
    ["slug:", "<value>"],
    ["text:", "<value>"],
  ]) {
    const row = document.createElement("div");
    row.className = "px-3 py-1.5 font-mono text-xs text-slate-500 dark:text-slate-400";
    const keywordSpan = document.createElement("span");
    keywordSpan.className = "text-indigo-600 dark:text-indigo-300";
    keywordSpan.textContent = keyword;
    row.append(keywordSpan, ` ${placeholder}`);
    dropdown.appendChild(row);
  }
}

async function runSearch(query) {
  const token = (searchRequestToken += 1);
  const response = await fetchJson(`/api/search?q=${encodeURIComponent(query)}`);
  if (token !== searchRequestToken) {
    return; // superseded by a newer keystroke
  }
  renderSearchResults(response.matches);
}

function renderSearchResults(matches) {
  const dropdown = document.getElementById("search-dropdown");
  dropdown.textContent = "";
  if (matches.length === 0) {
    const empty = document.createElement("div");
    empty.className = "px-3 py-3 text-xs text-slate-400 dark:text-slate-500";
    empty.textContent = "No matches";
    dropdown.appendChild(empty);
    return;
  }
  const showWorkspaceName = workspaces.size > 1;
  for (const match of matches) {
    const item = document.createElement("button");
    item.type = "button";
    item.className =
      "search-result-item flex w-full flex-col gap-0.5 px-3 py-1.5 text-left hover:bg-slate-100 dark:hover:bg-slate-800";
    const title = document.createElement("div");
    title.className = "truncate text-sm font-medium text-slate-800 dark:text-slate-100";
    title.textContent = match.title;
    const subtitle = document.createElement("div");
    subtitle.className = "truncate text-[11px] text-slate-500 dark:text-slate-400";
    subtitle.textContent = showWorkspaceName ? `${match.workspace_name} · ${match.path}` : match.path;
    item.append(title, subtitle);
    item.addEventListener("click", () => {
      focusSearchResult(match).catch(reportError);
    });
    dropdown.appendChild(item);
  }
  // A fresh set of results always starts highlighted on the first one, per
  // the requirement that arrow-key navigation lands there immediately —
  // without ever moving actual focus off the input (see moveSearchHighlight).
  applySearchHighlight(0);
}

function getSearchResultItems() {
  return Array.from(document.querySelectorAll(".search-result-item"));
}

function applySearchHighlight(index) {
  const items = getSearchResultItems();
  highlightedResultIndex = Math.max(0, Math.min(index, items.length - 1));
  items.forEach((item, itemIndex) => {
    item.classList.toggle("highlighted", itemIndex === highlightedResultIndex);
  });
  items[highlightedResultIndex]?.scrollIntoView({ block: "nearest" });
}

// Moves the highlight by `delta` results, clamped to the list's ends rather
// than wrapping — this never touches real DOM focus, which stays on the
// search input the entire time so typing is never interrupted.
function moveSearchHighlight(delta) {
  const items = getSearchResultItems();
  if (items.length === 0) {
    return;
  }
  applySearchHighlight(highlightedResultIndex + delta);
}

// Brings a search result into view: switches to its workspace (opening the
// panel item if more than one is loaded), expands every ancestor along its
// path (loading any not yet cached), selects it, and centers the camera on
// it — the tree-side equivalent of the "focus" molport.com's search jumps to.
async function focusSearchResult(match) {
  hideSearchPopover();
  if (match.workspace_id !== activeWorkspaceId) {
    await switchWorkspace(match.workspace_id);
  }
  for (const ancestorId of match.ancestor_ids) {
    state.expanded.add(ancestorId);
    if (!state.nodes.get(ancestorId)?.children) {
      await loadNode(activeWorkspaceId, ancestorId);
    }
  }
  setSelected(match.node_id);
  render();
  // Same zoom-to-max-if-collapsed, fit-to-bounds-if-expanded behavior as
  // enterFocusMode's own fitToNode call — a search jump should land close up
  // on a single result, not leave it looking tiny at whatever scale the
  // canvas happened to be at before the search.
  fitToNode(match.node_id, MAX_ZOOM);
}

// Brings a resolved reference's target into view: expands every ancestor
// along its path (loading any not yet cached), then selects and centers on
// it — the same "focus" sequence as focusSearchResult above, minus the
// workspace switch, since a Reference's target always lives in the same
// workspace as its source node.
async function goToReference(reference) {
  const targetId = reference.node_id;
  const { ancestor_ids: ancestorIds } = await fetchJson(
    `/api/${activeWorkspaceId}/node/${encodeURIComponent(targetId)}/ancestors`,
  );
  for (const ancestorId of ancestorIds) {
    state.expanded.add(ancestorId);
    if (!state.nodes.get(ancestorId)?.children) {
      await loadNode(activeWorkspaceId, ancestorId);
    }
  }
  if (!state.nodes.get(targetId)) {
    await loadNode(activeWorkspaceId, targetId);
  }
  setSelected(targetId);
  render();
  centerOnNode(targetId);
}

// Re-centers the camera on an already-rendered node without changing zoom,
// so a search result that was off-screen (or on a just-expanded branch)
// scrolls into view. A no-op if the id somehow still isn't in the current
// layout (e.g. mid-navigation race), rather than jumping to (0, 0).
function centerOnNode(id) {
  const canvas = document.getElementById("tree-canvas");
  const position = state.positions.get(id);
  if (!position) {
    return;
  }
  const rect = canvas.getBoundingClientRect();
  const scale = state.camera.scale || 1;
  state.camera.x = rect.width / 2 - position.x * scale;
  state.camera.y = rect.height / 2 - position.y * scale;
  applyCameraAnimated();
}

// Lays out the visible tree left-to-right: the root is pinned at a fixed
// top-left anchor (not centered over its descendants), each generation of
// expanded children cascading to its right, top to bottom, purely from
// canonical hierarchy and sibling order (no persisted coordinates). Siblings
// are spaced by their actual rendered card heights (see `cardHeightFor`,
// `topExtent`/`bottomExtent` below) so the visual gap between any two
// vertically adjacent cards is always exactly SLOT_GAP, whether or not
// either one is tall enough to have a relations row.
function layoutTree(root) {
  const positions = new Map();
  const edges = [];
  // Every node's extents are otherwise recomputed once per ancestor (place()
  // calls topExtent/bottomExtent for positioning, then recurses into
  // place(child, ...), which repeats the same work for its own children) —
  // an O(n * depth) blowup on a large visible scope. Caching per node id
  // makes one layoutTree() call strictly O(visible nodes).
  const topCache = new Map();
  const bottomCache = new Map();

  // How far above a node's own `y` its subtree's topmost content reaches.
  // A node's `y` always equals its first (visible) child's `y` — the
  // "leftmost spine" alignment `place` relies on below — so this is at
  // least inherited all the way down to whichever leaf anchors that spine;
  // but the node's own card shares that same `y`, so when its own height
  // exceeds what the spine leaf accounts for (e.g. a relations row makes it
  // taller than a childless first child), the node's own half-height must
  // win instead, or the card pokes out past the extent callers rely on.
  function topExtent(node) {
    const cached = topCache.get(node.id);
    if (cached !== undefined) {
      return cached;
    }
    const children = visibleChildren(node);
    const extent =
      children.length === 0
        ? cardHeightFor(node) / 2
        : Math.max(cardHeightFor(node) / 2, topExtent(children[0]));
    topCache.set(node.id, extent);
    return extent;
  }

  // How far below a node's own `y` its subtree's bottommost content reaches:
  // its own bottom half for a leaf, or — for a branch — as far as its own
  // half-height or its first child's own subtree reaches (whichever is
  // more, per the topExtent note above), plus every later sibling's full
  // subtree stacked below it with a fixed SLOT_GAP between each. Together
  // with topExtent, this is what lets `place` give every pair of vertically
  // adjacent cards the same gap regardless of how tall either one is (a
  // node with outgoing references is taller than one without — see
  // `cardHeightFor`).
  function bottomExtent(node) {
    const cached = bottomCache.get(node.id);
    if (cached !== undefined) {
      return cached;
    }
    const children = visibleChildren(node);
    let extent;
    if (children.length === 0) {
      extent = cardHeightFor(node) / 2;
    } else {
      extent = Math.max(cardHeightFor(node) / 2, bottomExtent(children[0]));
      for (let index = 1; index < children.length; index += 1) {
        extent += SLOT_GAP + topExtent(children[index]) + bottomExtent(children[index]);
      }
    }
    bottomCache.set(node.id, extent);
    return extent;
  }

  // `y` is this node's own, exact placement (not a slot to center within).
  // A node's first child always lands at that same `y` — so the "leftmost
  // spine" (first child of first child of first child...) stays on one
  // straight horizontal line with the root — while every later sibling is
  // offset below the previous one by exactly SLOT_GAP, measured from that
  // sibling's actual bottom edge to this one's actual top edge (via
  // bottomExtent/topExtent) rather than from its raw card height, which is
  // what kept the gap even when every card was the same height and broke it
  // once cards started varying (references add a relations row).
  function place(node, y, depth) {
    const x = ROW_START_X + depth * LEVEL_WIDTH;
    positions.set(node.id, { x, y, node });
    const children = visibleChildren(node);
    let previousChild = null;
    let previousY = y;
    for (const child of children) {
      edges.push({ from: node.id, to: child.id });
      const childY =
        previousChild === null
          ? y
          : previousY + bottomExtent(previousChild) + SLOT_GAP + topExtent(child);
      place(child, childY, depth + 1);
      previousChild = child;
      previousY = childY;
    }
    return y;
  }

  // The root is pinned at the fixed top-left anchor (its own height, like
  // every other node's, depends on whether it has references); everything
  // else (including its own first child, per `place` above) cascades from
  // there.
  place(root, CANVAS_MARGIN + cardHeightFor(root) / 2, 0);

  return { positions, edges };
}

function visibleChildren(node) {
  if (!state.expanded.has(node.id) || !Array.isArray(node.children)) {
    return [];
  }
  return node.children.map((summary) => state.nodes.get(summary.id) ?? summary);
}

// The root-to-selected-node ancestor chain, oldest first. Used to bold the
// active path's connecting edges while leaving every other edge at normal
// weight, so ancestry is traceable without losing surrounding context.
function ancestorPath(id) {
  const path = [];
  let current = id;
  while (current !== undefined) {
    path.unshift(current);
    current = state.parents.get(current);
  }
  return path;
}

function activePathEdgeKeys() {
  const keys = new Set();
  if (!state.selected) {
    return keys;
  }
  const path = ancestorPath(state.selected);
  for (let i = 0; i < path.length - 1; i += 1) {
    keys.add(`${path[i]}>${path[i + 1]}`);
  }
  return keys;
}

function render() {
  const world = document.getElementById("world");
  const edgesSvg = document.getElementById("edges-svg");
  for (const card of world.querySelectorAll(".node-card")) {
    card.remove();
  }
  edgesSvg.textContent = "";
  // These four don't actually depend on `root` resolving — set them
  // unconditionally, before the early return below. Otherwise a render()
  // that bails out early (root not resolved yet, however briefly) skips
  // every one of them, which is exactly how the "Expanding…" toast got
  // stuck on screen after finishing, and — the same bug — how the focus
  // mode banner could intermittently fail to appear right after entering
  // focus mode.
  document.getElementById("expand-status").hidden = !state.expanding;
  updateViewerStaleBanner();
  renderRelationsLegend();
  updateFocusModeBanner();
  refreshRootCurrentMenuTriggers();
  const root = state.nodes.get(state.root);
  if (!root) {
    return;
  }

  const { positions, edges } = layoutTree(root);
  const activeEdges = activePathEdgeKeys();
  // In focus mode, only this set stays sharp and interactive — everything
  // else (ancestors, siblings, unrelated branches) renders blurred and
  // inert, per "show only this branch". Not applied to the layout itself
  // (positions are unchanged) — only to how each card/edge is drawn — so
  // entering/exiting focus never reflows anything.
  const focusSet = focusedSubtreeIds();
  for (const edge of edges) {
    const from = positions.get(edge.from);
    const to = positions.get(edge.to);
    if (from && to) {
      const inFocus = !focusSet || (focusSet.has(edge.from) && focusSet.has(edge.to));
      drawEdge(edgesSvg, from, to, activeEdges.has(`${edge.from}>${edge.to}`), inFocus);
    }
  }
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const { x, y, node } of positions.values()) {
    const inFocus = !focusSet || focusSet.has(node.id);
    world.appendChild(buildNodeCard(node, x, y, inFocus));
    const height = cardHeightFor(node);
    minX = Math.min(minX, x - CARD_WIDTH / 2);
    minY = Math.min(minY, y - height / 2);
    maxX = Math.max(maxX, x + CARD_WIDTH / 2);
    maxY = Math.max(maxY, y + height / 2);
  }
  state.bounds = { minX, minY, maxX, maxY };
  state.positions = positions;
  applyCamera();
}

// The set of node ids that stay sharp in focus mode (the focused node plus
// its visible descendants), or null when not in focus mode at all — kept
// distinct from an empty/full set so callers can cheaply skip the "is this
// in scope" check entirely in the common (not focused) case.
function focusedSubtreeIds() {
  if (!state.focusedNodeId) {
    return null;
  }
  const ids = [];
  collectVisibleSubtreeIds(state.focusedNodeId, ids);
  return new Set(ids);
}

function applyCamera() {
  const world = document.getElementById("world");
  const { x, y, scale } = state.camera;
  world.style.transform = `translate(${x}px, ${y}px) scale(${scale})`;
}

// Duration of the eased camera jump used by Fit/Reset Zoom. Pointer-driven
// pan and zoom never use this: their feedback must appear within the same
// frame as the input, never after a transition delay.
const CAMERA_JUMP_MS = 180;
let cameraJumpTimeout = null;

// Cuts short any in-flight eased camera jump so the next pointer-driven pan
// or zoom starts from a plain, un-animated transform rather than fighting a
// still-running transition — every animation here must be interruptible.
function cancelCameraJump() {
  document.getElementById("world").classList.remove("animated");
  if (cameraJumpTimeout !== null) {
    clearTimeout(cameraJumpTimeout);
    cameraJumpTimeout = null;
  }
}

// Applies the camera with an eased transition, used only for discrete jumps
// (Fit/Reset Zoom, or a fit-to-subtree), never for live pointer feedback.
// Respects `prefers-reduced-motion` via CSS alone (the "animated" class's
// transition duration collapses to near-zero there — see tailwind.input.css).
function applyCameraAnimated(durationMs = CAMERA_JUMP_MS) {
  const world = document.getElementById("world");
  world.style.setProperty("--camera-jump-ms", `${durationMs}ms`);
  world.classList.add("animated");
  applyCamera();
  cameraJumpTimeout = setTimeout(() => {
    world.classList.remove("animated");
    cameraJumpTimeout = null;
  }, durationMs);
}

function clamp(value, min, max) {
  return Math.min(Math.max(value, min), max);
}

// Zooms so the point at (pointerX, pointerY) in canvas-relative coordinates
// stays fixed on screen, rather than zooming around the origin.
function zoomAt(pointerX, pointerY, factor) {
  const oldScale = state.camera.scale;
  const newScale = clamp(oldScale * factor, MIN_ZOOM, MAX_ZOOM);
  if (newScale === oldScale) {
    return;
  }
  const worldX = (pointerX - state.camera.x) / oldScale;
  const worldY = (pointerY - state.camera.y) / oldScale;
  state.camera.scale = newScale;
  state.camera.x = pointerX - worldX * newScale;
  state.camera.y = pointerY - worldY * newScale;
  applyCamera();
}

// Zooms in/out one discrete step, centered on the canvas's own middle
// (unlike `zoomAt`, which keeps an arbitrary pointer position fixed — right
// for continuous scroll/pinch feedback, but not what a button click wants)
// and animated the same way as Fit/Reset Zoom, since a button press is a
// discrete jump rather than continuous live feedback.
const ZOOM_BUTTON_FACTOR = 1.25;

function zoomByFactor(factor) {
  const oldScale = state.camera.scale;
  const newScale = clamp(oldScale * factor, MIN_ZOOM, MAX_ZOOM);
  if (newScale === oldScale) {
    return;
  }
  // Anchored on the selected node's own top-left corner (not its center,
  // and not the canvas's center) so whatever the user is looking at
  // visibly grows/shrinks in place from that fixed corner, instead of the
  // camera zooming around some other point and the selection drifting off
  // — its center moving as the card resizes would still read as
  // "shifting" even though the anchor point itself never moved. Falls back
  // to the root when nothing is selected (or the selection isn't currently
  // laid out), same as before this anchored on the selection.
  const anchorId = state.selected && state.positions.has(state.selected) ? state.selected : state.root;
  const anchorPosition = state.positions.get(anchorId);
  const rect = document.getElementById("tree-canvas").getBoundingClientRect();
  const worldX = anchorPosition
    ? anchorPosition.x - CARD_WIDTH / 2
    : (rect.width / 2 - state.camera.x) / oldScale;
  const worldY = anchorPosition
    ? anchorPosition.y - cardHeightFor(anchorPosition.node) / 2
    : (rect.height / 2 - state.camera.y) / oldScale;
  const anchorScreenX = state.camera.x + worldX * oldScale;
  const anchorScreenY = state.camera.y + worldY * oldScale;
  state.camera.scale = newScale;
  state.camera.x = anchorScreenX - worldX * newScale;
  state.camera.y = anchorScreenY - worldY * newScale;
  applyCameraAnimated();
}

function zoomIn() {
  zoomByFactor(ZOOM_BUTTON_FACTOR);
}

function zoomOut() {
  zoomByFactor(1 / ZOOM_BUTTON_FACTOR);
}

// Fits and centers all currently visible nodes within the canvas, with a
// margin, per the spec's `F` shortcut.
// The zoom level that fits the current bounds within the canvas (with a
// margin), shared by both fit modes below so they always agree on scale.
function computeFitScale(bounds, rect, maxScale) {
  const contentWidth = Math.max(bounds.maxX - bounds.minX, 1);
  const contentHeight = Math.max(bounds.maxY - bounds.minY, 1);
  const availableWidth = Math.max(rect.width - ZOOM_MARGIN * 2, 1);
  const availableHeight = Math.max(rect.height - ZOOM_MARGIN * 2, 1);
  return clamp(
    Math.min(availableWidth / contentWidth, availableHeight / contentHeight),
    MIN_ZOOM,
    maxScale,
  );
}

function fitToVisible(maxScale = MAX_ZOOM) {
  const canvas = document.getElementById("tree-canvas");
  const bounds = state.bounds;
  if (!bounds || !Number.isFinite(bounds.minX)) {
    return;
  }
  const rect = canvas.getBoundingClientRect();
  const scale = computeFitScale(bounds, rect, maxScale);
  const centerX = (bounds.minX + bounds.maxX) / 2;
  const centerY = (bounds.minY + bounds.maxY) / 2;
  state.camera.scale = scale;
  state.camera.x = rect.width / 2 - centerX * scale;
  state.camera.y = rect.height / 2 - centerY * scale;
  applyCameraAnimated();
}

// Anchors `bounds`' own top-left corner to the canvas's top-left corner
// (with a margin) instead of centering it — for pinning content to a fixed,
// predictable corner rather than wherever the middle of the canvas happens
// to be. Shared by `fitToTopLeft` (whole tree) and `fitToView` (selection).
function applyTopLeftFit(bounds, maxScale) {
  const canvas = document.getElementById("tree-canvas");
  const rect = canvas.getBoundingClientRect();
  const scale = computeFitScale(bounds, rect, maxScale);
  state.camera.scale = scale;
  // CANVAS_MARGIN (not ZOOM_MARGIN) on purpose: the root's very first,
  // never-fitted position is already pinned at exactly `CANVAS_MARGIN` from
  // the corner (see ROW_START_X/Y) — anchoring here to the same margin
  // means a fresh session and a "fit to top-left" after collapsing
  // everything land the root in the exact same spot, instead of a few
  // pixels apart because this used a different margin constant than the
  // layout itself does.
  state.camera.x = CANVAS_MARGIN - bounds.minX * scale;
  state.camera.y = CANVAS_MARGIN - bounds.minY * scale;
  applyCameraAnimated();
}

// Same fit-to-view scale as `fitToVisible`, but anchors the whole tree's
// top-left corner to the canvas's top-left corner instead of centering it.
// Used internally by "expand all"/"collapse all", where the whole tree
// (not just a selection) is what just changed shape.
function fitToTopLeft(maxScale = MAX_ZOOM) {
  const bounds = state.bounds;
  if (!bounds || !Number.isFinite(bounds.minX)) {
    return;
  }
  applyTopLeftFit(bounds, maxScale);
}

// The toolbar/keyboard "Fit" action: anchored top-left like `fitToTopLeft`,
// but scoped to the selected node's own subtree when one is selected —
// landing back on the branch you were just looking at is more useful than
// re-fitting the entire tree every time. Falls back to the whole tree when
// nothing is selected (or the selection isn't currently laid out).
function fitToView(maxScale) {
  const selectedBounds = state.selected && subtreeBounds(state.selected);
  const bounds = selectedBounds || state.bounds;
  if (!bounds || !Number.isFinite(bounds.minX)) {
    return;
  }
  // A single selected node (or a small subtree) fits into a much larger
  // scale than a whole expanded tree normally does, so — same as
  // control-close-all's own `fitToTopLeft(1)` — cap it at the default scale
  // rather than blowing a small selection up to fill the canvas. The whole
  // tree fallback keeps the usual uncapped MAX_ZOOM, matching
  // control-open-all.
  applyTopLeftFit(bounds, maxScale ?? (selectedBounds ? 1 : MAX_ZOOM));
}

// Restores the default zoom level (`0` shortcut) without changing what is
// centered.
function resetZoom() {
  const canvas = document.getElementById("tree-canvas");
  const rect = canvas.getBoundingClientRect();
  const bounds = state.bounds;
  const oldScale = state.camera.scale;
  const centerX = bounds ? (bounds.minX + bounds.maxX) / 2 : rect.width / 2;
  const centerY = bounds ? (bounds.minY + bounds.maxY) / 2 : rect.height / 2;
  const screenX = state.camera.x + centerX * oldScale;
  const screenY = state.camera.y + centerY * oldScale;
  state.camera.scale = 1;
  state.camera.x = screenX - centerX;
  state.camera.y = screenY - centerY;
  applyCameraAnimated();
}

// Reassigned by `setUpPanAndZoom()` to cancel whatever pan/drag gesture is
// in flight on the canvas; a no-op until then. Called on every workspace
// switch, since a gesture in progress belongs to whichever tree was visible
// when it started, not to whatever becomes active mid-gesture.
let cancelActiveGesture = () => {};

// How long after a plain click on a node card a second plain click on the
// *same* card counts as a double-click, hand-rolled below rather than
// relying on the browser's native `dblclick` event: `setPointerCapture` (see
// the pointerdown handler) retargets every subsequent pointer/mouse event,
// including the synthetic click/dblclick the browser would otherwise
// synthesize, to the canvas element itself rather than the card, so a
// listener on the card would never see it.
const DOUBLE_CLICK_MS = 400;

function setUpPanAndZoom() {
  const canvas = document.getElementById("tree-canvas");
  let panState = null;
  let dragState = null;
  let lastCardClick = null;

  cancelActiveGesture = () => {
    if (panState) {
      panState = null;
      canvas.classList.remove("panning");
    }
    if (dragState) {
      if (dragState.moved) {
        dragState.card.classList.remove("drag-source");
        dragState.card.style.transform = "";
        clearInsertionMarker();
        clearDropTargetClasses();
      }
      dragState = null;
    }
  };

  canvas.addEventListener("pointerdown", (event) => {
    // Floating overlays on the canvas (the focus-mode banner and its exit
    // button, the expand-all status toast) aren't part of the pannable
    // surface — without this, starting a click there is indistinguishable
    // from starting a pan, and capturing the pointer for panning steals the
    // click before it ever reaches a button inside one of them.
    if (event.target.closest("#focus-mode-banner") || event.target.closest("#expand-status")) {
      return;
    }
    const card = event.target.closest(".node-card");
    if (!card) {
      cancelCameraJump();
      panState = {
        pointerId: event.pointerId,
        startX: event.clientX,
        startY: event.clientY,
        originX: state.camera.x,
        originY: state.camera.y,
      };
      canvas.setPointerCapture(event.pointerId);
      canvas.classList.add("panning");
      return;
    }
    // The disclosure-toggling icon, reload, more-actions, and reference dots
    // are small dedicated controls with their own click handling; everywhere
    // else on the card selects on a plain click and moves the node once the
    // pointer actually travels (drag-to-reorder/reparent). Without this,
    // capturing the pointer here for drag tracking would redirect the
    // reference dot's own "click" to the canvas instead — its hover tooltip
    // would still work (pointerenter/pointerleave aren't affected by
    // capture), but a click would never reach the dot's listener.
    if (
      event.target.closest(".node-card-icon-toggle") ||
      event.target.closest(".node-card-reload") ||
      event.target.closest(".node-card-more") ||
      event.target.closest(".node-card-relation-dot")
    ) {
      return;
    }
    const id = card.dataset.id;
    const parentId = state.parents.get(id);
    const position = state.positions.get(id);
    // The root has no parent and cannot be reordered as a child; a stale
    // node cannot be dragged; only one structural command at a time;
    // structural edits require a live connection to the server. None of
    // this blocks a plain click, only whether *movement* may become a
    // structural command.
    const dragEligible =
      Boolean(parentId) && Boolean(position) && !state.stale.has(id) && !state.dragPending && isConnected();
    dragState = {
      pointerId: event.pointerId,
      id,
      parentId,
      card,
      startClientX: event.clientX,
      startClientY: event.clientY,
      originX: position ? position.x : 0,
      originY: position ? position.y : 0,
      moved: false,
      targetIndex: 0,
      othersCount: 0,
      dropTargetId: null,
      dragEligible,
    };
    canvas.setPointerCapture(event.pointerId);
  });

  canvas.addEventListener("pointermove", (event) => {
    if (panState && event.pointerId === panState.pointerId) {
      state.camera.x = panState.originX + (event.clientX - panState.startX);
      state.camera.y = panState.originY + (event.clientY - panState.startY);
      applyCamera();
      return;
    }
    if (!dragState || event.pointerId !== dragState.pointerId) {
      return;
    }
    if (
      !dragState.moved &&
      Math.hypot(event.clientX - dragState.startClientX, event.clientY - dragState.startClientY) < 4
    ) {
      return;
    }
    if (!dragState.dragEligible) {
      // Past the movement threshold, but this node can't be dragged (root,
      // stale, disconnected, or a command already in flight) — mark it
      // moved so releasing the pointer is treated as an abandoned gesture,
      // not a click-to-select, without visually dragging anything.
      dragState.moved = true;
      return;
    }
    const dx = (event.clientX - dragState.startClientX) / state.camera.scale;
    const dy = (event.clientY - dragState.startClientY) / state.camera.scale;
    if (!dragState.moved) {
      dragState.moved = true;
      dragState.card.classList.add("drag-source");
    }
    dragState.card.style.transform = `translate(${dx}px, ${dy}px)`;
    const worldX = dragState.originX + dx;
    const worldY = dragState.originY + dy;
    const hovered = findDropTarget(worldX, worldY, dragState.id);
    updateDropTargetFeedback(dragState, hovered);
    if (dragState.dropTargetId) {
      clearInsertionMarker();
    } else {
      updateInsertionMarker(dragState, worldY);
    }
  });

  const endPointer = (event) => {
    if (panState && event.pointerId === panState.pointerId) {
      panState = null;
      canvas.classList.remove("panning");
      return;
    }
    if (!dragState || event.pointerId !== dragState.pointerId) {
      return;
    }
    const finished = dragState;
    dragState = null;
    if (!finished.moved) {
      const now = Date.now();
      const isDoubleClick =
        lastCardClick && lastCardClick.id === finished.id && now - lastCardClick.time <= DOUBLE_CLICK_MS;
      if (isDoubleClick) {
        // Consumed — a third click right after starts a fresh pair rather
        // than re-triggering immediately.
        lastCardClick = null;
        toggleExpand(finished.id).catch(reportError);
        return;
      }
      lastCardClick = { id: finished.id, time: now };
      // A plain click (no movement past the threshold) only selects the
      // node. Markdown is opened exclusively through keyboard shortcuts.
      setSelected(finished.id);
      return;
    }
    if (!finished.dragEligible) {
      // Moved, but this node was never actually draggable — nothing was
      // visually dragged and nothing was committed.
      return;
    }
    finished.card.classList.remove("drag-source");
    finished.card.style.transform = "";
    clearInsertionMarker();
    clearDropTargetClasses();
    if (finished.dropTargetId) {
      commitMove(finished);
    } else {
      commitReorder(finished);
    }
  };
  canvas.addEventListener("pointerup", endPointer);
  canvas.addEventListener("pointercancel", endPointer);

  canvas.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      cancelCameraJump();
      const rect = canvas.getBoundingClientRect();
      const factor = 2 ** (-event.deltaY * 0.001);
      zoomAt(event.clientX - rect.left, event.clientY - rect.top, factor);
    },
    { passive: false },
  );
}

// Hit-tests the pointer's current tree-space position against every other
// visible node's card bounds, for proposing it as a new parent.
function findDropTarget(worldX, worldY, excludeId) {
  for (const [id, position] of state.positions) {
    if (id === excludeId) {
      continue;
    }
    const height = cardHeightFor(position.node);
    const left = position.x - CARD_WIDTH / 2;
    const right = position.x + CARD_WIDTH / 2;
    const top = position.y - height / 2;
    const bottom = position.y + height / 2;
    if (worldX >= left && worldX <= right && worldY >= top && worldY <= bottom) {
      return id;
    }
  }
  return null;
}

// True when `candidateId` is `ancestorId` itself or a descendant of it,
// walking up the (loaded) parent chain. Invalid reparent destinations
// (the dragged node itself, or any of its own descendants) are computed
// this way rather than requiring the whole subtree to be loaded.
function isDescendantOrSelf(candidateId, ancestorId) {
  let current = candidateId;
  while (current !== undefined) {
    if (current === ancestorId) {
      return true;
    }
    current = state.parents.get(current);
  }
  return false;
}

// Shows the destination's valid/invalid sticking state as the pointer moves
// over it; only a valid target ever sticks (gets recorded as the drop
// target). Entering and leaving a region toggles the visual state without
// mutating anything.
function updateDropTargetFeedback(dragState, hoveredId) {
  clearDropTargetClasses();
  if (!hoveredId) {
    dragState.dropTargetId = null;
    return;
  }
  const invalid = state.stale.has(hoveredId) || isDescendantOrSelf(hoveredId, dragState.id);
  const card = document.querySelector(`.node-card[data-id="${hoveredId}"]`);
  card?.classList.add(invalid ? "invalid-drop-target" : "valid-drop-target");
  dragState.dropTargetId = invalid ? null : hoveredId;
}

function clearDropTargetClasses() {
  for (const el of document.querySelectorAll(".valid-drop-target, .invalid-drop-target")) {
    el.classList.remove("valid-drop-target", "invalid-drop-target");
  }
}

// Sends the move_subtree command for a completed reparent drag; like
// commitReorder, the layout only settles once the server responds.
function commitMove(finished) {
  const node = state.nodes.get(finished.id);
  const summary = (state.nodes.get(finished.parentId)?.children ?? []).find(
    (child) => child.id === finished.id,
  );
  const version = node?.version ?? summary?.version;
  if (version === undefined) {
    render();
    return;
  }

  state.dragPending = {
    id: finished.id,
    command: "move_subtree",
    // Both the old and new parent's cached children lists are now wrong
    // (one still lists the moved node, the other doesn't yet) and need
    // refetching once the server confirms.
    affectedParentIds: [finished.parentId, finished.dropTargetId],
  };
  state.loading.add(finished.id);
  render();

  const sent = sendCommand("move_subtree", {
    selector: finished.id,
    new_parent: finished.dropTargetId,
    expected_version: version,
  });
  if (!sent) {
    state.loading.delete(finished.id);
    state.dragPending = null;
    render();
  }
}

// Computes which sibling slot the drag would land in if released now (a
// count of other siblings whose position is above the dragged node's
// current position) and shows a stable insertion gap there, without
// changing any other node's rendered position.
function updateInsertionMarker(dragState, currentWorldY) {
  const parent = state.nodes.get(dragState.parentId);
  const others = (parent?.children ?? []).filter((child) => child.id !== dragState.id);
  let index = 0;
  for (const other of others) {
    const position = state.positions.get(other.id);
    if (position && position.y < currentWorldY) {
      index += 1;
    }
  }
  dragState.targetIndex = index;
  dragState.othersCount = others.length;

  let markerY;
  if (others.length === 0) {
    markerY = currentWorldY;
  } else if (index === 0) {
    const first = state.positions.get(others[0].id);
    markerY = first.y - (cardHeightFor(first.node) / 2 + SLOT_GAP / 2);
  } else if (index >= others.length) {
    const last = state.positions.get(others[others.length - 1].id);
    markerY = last.y + (cardHeightFor(last.node) / 2 + SLOT_GAP / 2);
  } else {
    const above = state.positions.get(others[index - 1].id).y;
    const below = state.positions.get(others[index].id).y;
    markerY = (above + below) / 2;
  }
  showInsertionMarker(dragState.originX, markerY);
}

function showInsertionMarker(x, y) {
  const svg = document.getElementById("edges-svg");
  let marker = document.getElementById("insertion-marker");
  if (!marker) {
    marker = document.createElementNS(SVG_NS, "line");
    marker.setAttribute("id", "insertion-marker");
    marker.classList.add("insertion-marker");
    svg.appendChild(marker);
  }
  marker.setAttribute("x1", String(x - CARD_WIDTH / 2 - 6));
  marker.setAttribute("x2", String(x + CARD_WIDTH / 2 + 6));
  marker.setAttribute("y1", String(y));
  marker.setAttribute("y2", String(y));
}

function clearInsertionMarker() {
  document.getElementById("insertion-marker")?.remove();
}

// Sends the reorder command for a completed drag and marks it pending; the
// layout only settles (via a data refetch) once the server acknowledges or
// rejects it — never an optimistic local reorder.
function commitReorder(finished) {
  const parent = state.nodes.get(finished.parentId);
  const summary = (parent?.children ?? []).find((child) => child.id === finished.id);
  if (!summary) {
    render();
    return;
  }
  // `targetIndex` (see updateInsertionMarker) counts how many *other*
  // siblings sit above wherever the drag ended — which is exactly the
  // dragged node's own index in the parent's original children array
  // (everything before it there is, by definition, "others above it").
  // Released back at that same slot — including never having crossed any
  // other sibling's midpoint, however far it visually wandered — is not a
  // reorder at all: skip the round trip rather than sending a same-position
  // mutation that would still bump the node's version and mark the parent
  // stale for every connected client, for a move that never happened.
  const originalIndex = parent.children.indexOf(summary);
  if (finished.targetIndex === originalIndex) {
    render();
    return;
  }
  state.dragPending = {
    id: finished.id,
    command: "reorder_node",
    affectedParentIds: [finished.parentId],
  };
  state.loading.add(finished.id);
  render();

  const sent = sendCommand("reorder_node", {
    selector: finished.id,
    sibling_order: finished.targetIndex,
    expected_version: summary.version,
  });
  if (!sent) {
    state.loading.delete(finished.id);
    state.dragPending = null;
    render();
  }
}

// Dispatches one ack/reject envelope to whichever pending command it
// answers. Drag (reorder/move), edit-save, and create-child each keep their
// own pending-command slot on `state` so an envelope answering one can never
// be misapplied to another that happens to be in flight at the same time.
async function handleCommandResponse(envelope) {
  const command = envelope.payload?.command;
  if (state.dragPending && command === state.dragPending.command) {
    await handleDragCommandResponse(envelope);
  } else if (state.pendingEdit && command === "update_node") {
    await handleUpdateNodeResponse(envelope);
  } else if (state.pendingCreate && command === "create_node") {
    await handleCreateNodeResponse(envelope);
  } else if (state.pendingDelete && command === "remove_node") {
    await handleRemoveNodeResponse(envelope);
  }
}

async function handleDragCommandResponse(envelope) {
  const pending = state.dragPending;
  state.dragPending = null;
  state.loading.delete(pending.id);
  if (envelope.type === "ack") {
    noteSelfCausedChange();
    // Refetching every affected parent (for reorder: the one unchanged
    // parent; for move: both the old and new parent) is what settles the
    // layout into whatever the server actually applied, never a local guess.
    // Flagged stale (rather than just quietly refreshed) the same way a
    // create flags the parent it added to — a visible "this just changed"
    // signal on exactly the node(s) whose children list changed, cleared by
    // the next manual reload.
    for (const parentId of pending.affectedParentIds) {
      await loadNode(activeWorkspaceId, parentId);
      state.stale.add(parentId);
    }
  } else {
    // A conflicting command (a concurrent mutation changed the version this
    // drag observed) is never surfaced as an error dialog or toast: the
    // dragged node already visually returned to its prior position when the
    // pointer was released, and marking it (and the proposed destination,
    // for a reparent) stale is the only feedback. The user reloads and
    // retries; nothing here guesses at what actually changed.
    state.stale.add(pending.id);
    for (const parentId of pending.affectedParentIds) {
      state.stale.add(parentId);
    }
  }
  render();
}

// Draws one parent-to-child connector as a horizontal S-curve from the
// parent card's right edge to the child card's left edge, matching a
// left-to-right node-editor layout.
function drawEdge(svg, from, to, active, inFocus = true) {
  const path = document.createElementNS(SVG_NS, "path");
  path.classList.add("edge-path");
  if (active) {
    path.classList.add("active");
  }
  if (!inFocus) {
    path.classList.add("out-of-focus");
  }
  const startX = from.x + CARD_WIDTH / 2;
  const endX = to.x - CARD_WIDTH / 2;
  const midX = (startX + endX) / 2;
  path.setAttribute(
    "d",
    `M ${startX} ${from.y} C ${midX} ${from.y}, ${midX} ${to.y}, ${endX} ${to.y}`,
  );
  svg.appendChild(path);
}

// Leaf nodes (no children) get a document icon; nodes with children get a
// folder icon, so a parent is never visually confused with a leaf at a
// glance — independent of the child-count badge. The folder itself doubles
// as the expand/collapse control (see buildNodeCard), so it swaps between a
// closed and an open silhouette depending on `state.expanded`, the same way
// a classic Windows Explorer folder icon does, rather than needing a
// separate disclosure indicator alongside it.
// Sourced verbatim from Heroicons (MIT licensed, 24x24 outline set:
// document/folder/folder-open) rather than hand-drawn, so the shapes are
// unambiguous at a glance instead of a rough approximation. All three share
// the same viewBox/stroke-width (see buildNodeCard) for consistent visual
// weight when the folder icon swaps between its two states.
const DOCUMENT_ICON_PATH =
  "M19.5 14.25V11.625C19.5 9.76104 17.989 8.25 16.125 8.25H14.625C14.0037 8.25 13.5 7.74632 13.5 7.125V5.625C13.5 3.76104 11.989 2.25 10.125 2.25H8.25M10.5 2.25H5.625C5.00368 2.25 4.5 2.75368 4.5 3.375V20.625C4.5 21.2463 5.00368 21.75 5.625 21.75H18.375C18.9963 21.75 19.5 21.2463 19.5 20.625V11.25C19.5 6.27944 15.4706 2.25 10.5 2.25Z";
const FOLDER_CLOSED_ICON_PATH =
  "M2.25 12.75V12C2.25 10.7574 3.25736 9.75 4.5 9.75H19.5C20.7426 9.75 21.75 10.7574 21.75 12V12.75M13.0607 6.31066L10.9393 4.18934C10.658 3.90804 10.2765 3.75 9.87868 3.75H4.5C3.25736 3.75 2.25 4.75736 2.25 6V18C2.25 19.2426 3.25736 20.25 4.5 20.25H19.5C20.7426 20.25 21.75 19.2426 21.75 18V9C21.75 7.75736 20.7426 6.75 19.5 6.75H14.1213C13.7235 6.75 13.342 6.59197 13.0607 6.31066Z";
const FOLDER_OPEN_ICON_PATH =
  "M3.74999 9.77602C3.86203 9.7589 3.97698 9.75 4.09426 9.75H19.9057C20.023 9.75 20.138 9.7589 20.25 9.77602M3.74999 9.77602C2.55399 9.9588 1.68982 11.0788 1.86688 12.3182L2.72402 18.3182C2.88237 19.4267 3.83169 20.25 4.95141 20.25H19.0486C20.1683 20.25 21.1176 19.4267 21.276 18.3182L22.1331 12.3182C22.3102 11.0788 21.446 9.9588 20.25 9.77602M3.74999 9.77602V6C3.74999 4.75736 4.75735 3.75 5.99999 3.75H9.87867C10.2765 3.75 10.658 3.90804 10.9393 4.18934L13.0607 6.31066C13.342 6.59197 13.7235 6.75 14.1213 6.75H18C19.2426 6.75 20.25 7.75736 20.25 9V9.77602";

// Id of the node a `#node-card-menu` action applies to, set by whichever
// card's "more actions" trigger last opened it, read by the menu's own two
// item buttons (see the click listeners set up near the bottom of this
// file) and cleared on close.
let nodeCardMenuTargetId = null;

// Shared by every card rather than one popover per card — a large tree would
// otherwise mean one hidden popover element per node for no benefit, since
// only one can ever be open at a time.
function showNodeCardMenu(trigger, nodeId) {
  nodeCardMenuTargetId = nodeId;
  const menu = document.getElementById("node-card-menu");
  const focusItem = document.getElementById("node-card-menu-focus");
  const deleteItem = document.getElementById("node-card-menu-delete");
  // "Show only this branch" on the root would just show the whole tree —
  // the same as not focusing at all — so it's omitted for the root instead
  // of offered as a no-op. The root can't be deleted either (see
  // requestDeleteNode), so its delete action is omitted the same way.
  focusItem.hidden = nodeId === state.root;
  deleteItem.hidden = nodeId === state.root;
  const rect = trigger.getBoundingClientRect();
  // A row of icon-rail-button chips (see index.html), not the wider
  // text-label list this used to be — narrow enough that even three of
  // them plus padding/gaps comfortably fits the smaller assumed width.
  const assumedMenuWidth = 140;
  const overflowsRight = rect.right + 8 + assumedMenuWidth > window.innerWidth;
  menu.style.left = `${Math.max(8, overflowsRight ? rect.left - assumedMenuWidth - 8 : rect.right + 8)}px`;
  menu.style.top = `${rect.top}px`;
  menu.hidden = false;
}

function hideNodeCardMenu() {
  document.getElementById("node-card-menu").hidden = true;
  hideTooltip();
  nodeCardMenuTargetId = null;
}

function buildNodeCard(node, x, y, inFocus = true) {
  const isStale = state.stale.has(node.id);
  const isLoading = state.loading.has(node.id);

  const card = document.createElement("div");
  card.classList.add("node-card");
  if (state.selected === node.id) {
    card.classList.add("selected");
  }
  if (isStale) {
    card.classList.add("stale");
  }
  if (isLoading) {
    card.classList.add("loading");
  }
  if (!inFocus) {
    card.classList.add("out-of-focus");
  }
  const height = cardHeightFor(node);
  card.dataset.id = node.id;
  card.style.left = `${x - CARD_WIDTH / 2}px`;
  card.style.top = `${y - height / 2}px`;
  card.style.width = `${CARD_WIDTH}px`;
  card.style.height = `${height}px`;

  const header = document.createElement("div");
  header.className = "flex items-center gap-1.5 px-2.5 pt-2";

  // The disclosure control is folded into the type icon itself, rather than
  // a separate element next to it, so expand/collapse costs no header width
  // of its own; a leaf's document icon isn't clickable, exactly as a leaf
  // previously had no disclosure control at all.
  const hasChildren = node.children_count > 0;
  const expanded = hasChildren && state.expanded.has(node.id);
  const icon = document.createElement("div");
  icon.className =
    "flex h-6 w-6 flex-none items-center justify-center bg-indigo-500/15 text-indigo-600 dark:bg-indigo-500/20 dark:text-indigo-300" +
    (hasChildren ? " cursor-pointer" : "");
  const iconPath = hasChildren
    ? (expanded ? FOLDER_OPEN_ICON_PATH : FOLDER_CLOSED_ICON_PATH)
    : DOCUMENT_ICON_PATH;
  icon.innerHTML = `<svg viewBox="0 0 24 24" fill="none" class="h-3.5 w-3.5" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round"><path d="${iconPath}" /></svg>`;
  if (hasChildren) {
    icon.classList.add("node-card-icon-toggle");
    icon.setAttribute("role", "button");
    icon.tabIndex = 0;
    icon.title = expanded ? "Collapse" : "Expand";
    icon.setAttribute("aria-label", expanded ? "Collapse" : "Expand");
    icon.addEventListener("click", (event) => {
      event.stopPropagation();
      toggleExpand(node.id).catch(reportError);
    });
  }
  header.appendChild(icon);

  const titleBlock = document.createElement("div");
  titleBlock.className = "min-w-0 flex-1";
  // No click handler of its own: selection and dragging are handled
  // uniformly for the whole card body by the canvas-level pointer handlers.
  // The folder/document icon (see FOLDER_CLOSED_ICON_PATH/DOCUMENT_ICON_PATH above)
  // already distinguishes a parent from a leaf, so this no longer needs its
  // own "has children"/"leaf" subtitle line — one fewer row buys back the
  // height a smaller card needs.
  const title = document.createElement("div");
  title.className = "node-card-title";
  title.textContent = node.title;
  titleBlock.append(title);
  header.appendChild(titleBlock);

  // Reload and "more actions" are the header's two small trailing icon
  // buttons — grouped in their own tightly-spaced wrapper (rather than
  // sitting at the header's own wider gap-1.5, like the type icon and
  // title) so they read as one cluster of card-level controls.
  const actions = document.createElement("div");
  actions.className = "flex flex-none items-center gap-0.5";

  // Present whenever a node is stale (CSS hides it otherwise via the
  // `invisible` utility, which reserves layout space), so it never changes
  // the card's size when toggled.
  const reload = document.createElement("button");
  reload.type = "button";
  reload.className = "node-card-reload";
  reload.title = "Reload";
  reload.setAttribute("aria-label", "Reload");
  reload.innerHTML =
    '<svg viewBox="0 0 20 20" fill="none" class="h-3.5 w-3.5" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M4 10a6 6 0 0 1 10.5-4M16 10a6 6 0 0 1-10.5 4M14 3v3.5H10.5M6 17v-3.5H9.5" /></svg>';
  reload.addEventListener("click", (event) => {
    event.stopPropagation();
    reloadNode(node.id).catch(reportError);
  });
  actions.appendChild(reload);

  // A single hover-revealed "more actions" trigger (see .node-card:hover
  // .node-card-more) rather than one bare icon button per action — "Show
  // only this branch" and "Add child node" are both deliberate, occasional
  // actions, not things every card needs to advertise permanently, and
  // reserving a separate icon slot per action left too little room for the
  // title itself. One trigger opens the shared #node-card-menu popover (see
  // showNodeCardMenu) with both as labeled items instead.
  const moreButton = document.createElement("button");
  moreButton.type = "button";
  moreButton.className = "node-card-more";
  moreButton.title = "More actions";
  moreButton.setAttribute("aria-label", "More actions");
  moreButton.addEventListener("click", (event) => {
    event.stopPropagation();
    // Opening the menu also selects this card — its actions (delete, add
    // child, focus) all act on "the selected node" elsewhere in the UI, so
    // opening the menu without selecting first would apply them to whatever
    // was already selected instead of the card whose trigger was actually
    // clicked. setSelected re-renders every card from scratch, so `moreButton`
    // itself is stale immediately after — re-fetch the freshly rendered
    // trigger for this same node before handing it to showNodeCardMenu.
    setSelected(node.id);
    const refreshedTrigger = document.querySelector(`.node-card[data-id="${node.id}"] .node-card-more`);
    showNodeCardMenu(refreshedTrigger ?? moreButton, node.id);
  });
  moreButton.innerHTML =
    '<svg viewBox="0 0 20 20" class="h-3.5 w-3.5"><circle cx="10" cy="5" r="1.5" fill="currentColor" /><circle cx="10" cy="10" r="1.5" fill="currentColor" /><circle cx="10" cy="15" r="1.5" fill="currentColor" /></svg>';
  actions.appendChild(moreButton);
  header.appendChild(actions);

  card.appendChild(header);

  card.appendChild(document.createElement("div")).className = "mx-2.5 mt-1.5 border-t border-slate-200 dark:border-slate-700";

  const fieldBlock = document.createElement("div");
  fieldBlock.className = "px-2.5 pt-1.5";
  const fieldLabel = document.createElement("div");
  fieldLabel.className = "mb-0.5 text-[10px] font-medium uppercase tracking-wide text-slate-500";
  fieldLabel.textContent = "Slug";
  const fieldValue = document.createElement("div");
  fieldValue.className =
    "truncate rounded-sm border border-slate-200 bg-slate-100/60 px-2 py-0.5 font-mono text-xs text-slate-600 dark:border-slate-700 dark:bg-slate-900/60 dark:text-slate-300";
  fieldValue.textContent = node.slug ?? "";
  fieldBlock.append(fieldLabel, fieldValue);
  card.appendChild(fieldBlock);

  // Below the slug, labeled and separated by its own divider the same way
  // the slug itself is labeled and separated from the header above — rather
  // than sitting unlabeled right under the title/icon row, where the dots
  // were easy to miss or misclick. Omitted entirely (divider included) when
  // the node has no outgoing references at all, rather than showing an
  // empty labeled row with nothing under it.
  if (node.references?.length) {
    card.appendChild(document.createElement("div")).className = "mx-2.5 mt-1.5 border-t border-slate-200 dark:border-slate-700";

    const relationsBlock = document.createElement("div");
    relationsBlock.className = "px-2.5 pt-1.5 pb-2";
    const relationsLabel = document.createElement("div");
    relationsLabel.className = "mb-0.5 text-[10px] font-medium uppercase tracking-wide text-slate-500";
    relationsLabel.textContent = "Relations";
    const relationsRow = document.createElement("div");
    relationsRow.className = "flex flex-wrap items-center gap-1.5";
    // One dot per actual outgoing reference (not deduplicated by type) — a
    // node with two `depends_on` references to two different targets gets
    // two dots, each hoverable and clickable on its own. Uses a custom
    // tooltip (showTooltip/hideTooltip) rather than the native `title`
    // attribute so sweeping across several adjacent dots switches the
    // tooltip immediately instead of waiting out the native show delay for
    // each one. The `node-card-relation-dot` class opts each dot out of the
    // canvas's own pointerdown drag/pan handling (see setUpPanAndZoom), the
    // same way `.node-card-icon-toggle`/`-reload`/`-more` already do —
    // otherwise the canvas would capture the pointer before the dot's own
    // click ever fires.
    for (const reference of node.references) {
      const dot = document.createElement("span");
      const resolved = reference.status === "resolved";
      dot.className = `node-card-relation-dot h-2 w-2 flex-none ${colorForRelation(state, reference.reference_type)} ${
        resolved ? "cursor-pointer" : "cursor-default opacity-50"
      }`;
      const label = resolved
        ? `${capitalize(reference.reference_type)} → ${reference.title}`
        : `${capitalize(reference.reference_type)} → ${reference.target_ref} (unresolved)`;
      dot.addEventListener("pointerenter", () => showTooltip(dot, label, "above"));
      dot.addEventListener("pointerleave", hideTooltip);
      if (resolved) {
        dot.addEventListener("click", (event) => {
          event.stopPropagation();
          hideTooltip();
          goToReference(reference).catch(reportError);
        });
      }
      relationsRow.appendChild(dot);
    }
    relationsBlock.append(relationsLabel, relationsRow);
    card.appendChild(relationsBlock);
  }

  // Child-count badge, pinned to the card's corner. `.node-card` is
  // `position: absolute`, so this positions relative to the card itself
  // without any extra wrapper.
  if (node.children_count > 0) {
    const badge = document.createElement("div");
    badge.className =
      "absolute -top-2 -right-2 flex h-5 min-w-5 items-center justify-center rounded-full border border-white bg-indigo-500 px-1 text-[10px] font-semibold text-white shadow dark:border-slate-900";
    badge.textContent = String(node.children_count);
    badge.setAttribute("aria-label", `${node.children_count} children`);
    card.appendChild(badge);
  }

  return card;
}

async function toggleExpand(id) {
  if (state.stale.has(id)) {
    return;
  }
  if (state.expanded.has(id)) {
    state.expanded.delete(id);
    render();
  } else {
    state.expanded.add(id);
    if (!state.nodes.get(id)?.children) {
      await loadNode(activeWorkspaceId, id);
    }
    // Opening a node is also a natural moment to make it the selection —
    // setSelected's own render() covers the expand too, so this doesn't
    // need a separate render() call of its own.
    setSelected(id);
  }
  // A node being expanded re-centers over its newly-visible children (and
  // pushes every sibling below it further down); collapsing does the
  // reverse. For a subtree with only a few children, simply following the
  // toggled node would be enough — but for one with many (or several levels
  // already expanded beneath it), the node itself can stay on screen while
  // most of its children still span far outside the viewport at the current
  // zoom: edges cross the screen toward them, but the cards themselves are
  // nowhere in view, which reads as content having vanished. So this checks
  // (and if needed, fits to) the *whole* visible subtree, not just the one
  // node — zooming out only if the subtree doesn't already fit, never in.
  if (!isSubtreeInViewport(id)) {
    fitToNode(id, state.camera.scale);
  }
}

// Collects `id` and every currently-visible (expanded) descendant id below
// it, in the same traversal `layoutTree`/`visibleChildren` use — i.e.
// exactly the set of cards actually on the canvas for this node's subtree.
function collectVisibleSubtreeIds(id, into) {
  into.push(id);
  const node = state.nodes.get(id);
  if (!node || !state.expanded.has(id)) {
    return;
  }
  for (const child of visibleChildren(node)) {
    collectVisibleSubtreeIds(child.id, into);
  }
}

// The bounding box (in unscaled tree coordinates) of every currently-laid-out
// card in `id`'s visible subtree, or null if none of them have a position
// yet (shouldn't happen for an already-rendered node, but guards against a
// mid-navigation race rather than computing bounds from nothing).
function subtreeBounds(id) {
  const ids = [];
  collectVisibleSubtreeIds(id, ids);
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const nodeId of ids) {
    const position = state.positions.get(nodeId);
    if (!position) {
      continue;
    }
    const height = cardHeightFor(position.node);
    minX = Math.min(minX, position.x - CARD_WIDTH / 2);
    minY = Math.min(minY, position.y - height / 2);
    maxX = Math.max(maxX, position.x + CARD_WIDTH / 2);
    maxY = Math.max(maxY, position.y + height / 2);
  }
  return Number.isFinite(minX) ? { minX, minY, maxX, maxY } : null;
}

// Whether every card in `id`'s visible subtree currently falls within the
// tree-canvas area at the current camera pan/zoom.
function isSubtreeInViewport(id) {
  const bounds = subtreeBounds(id);
  if (!bounds) {
    return false;
  }
  const rect = document.getElementById("tree-canvas").getBoundingClientRect();
  const scale = state.camera.scale || 1;
  return (
    state.camera.x + bounds.minX * scale >= 0 &&
    state.camera.y + bounds.minY * scale >= 0 &&
    state.camera.x + bounds.maxX * scale <= rect.width &&
    state.camera.y + bounds.maxY * scale <= rect.height
  );
}

// A visibly slower jump than Fit/Reset Zoom's snappy 180ms — this one is
// triggered automatically (not from a direct click on Fit), so it needs to
// read unambiguously as "zooming out to reveal everything" rather than a
// blink-and-you-miss-it jump cut, per the requested "zoom out, land on fit"
// motion for a branch that fell out of view.
const SUBTREE_FIT_JUMP_MS = 420;

// Pans (and, only if the subtree doesn't already fit, zooms out — never in,
// since `maxScale` is normally the camera's own current scale) to bring
// `id`'s entire visible subtree into view, the same fit-scale math as
// `fitToVisible` but scoped to one node's descendants instead of the whole
// tree.
function fitToNode(id, maxScale = MAX_ZOOM) {
  const bounds = subtreeBounds(id);
  if (!bounds) {
    return;
  }
  const canvas = document.getElementById("tree-canvas");
  const rect = canvas.getBoundingClientRect();
  const scale = computeFitScale(bounds, rect, maxScale);
  const centerX = (bounds.minX + bounds.maxX) / 2;
  const centerY = (bounds.minY + bounds.maxY) / 2;
  state.camera.scale = scale;
  state.camera.x = rect.width / 2 - centerX * scale;
  state.camera.y = rect.height / 2 - centerY * scale;
  applyCameraAnimated(SUBTREE_FIT_JUMP_MS);
}

// "Show only this branch": isolates one node's subtree by blurring and
// disabling every other card/edge (see render()'s focusSet handling), and
// fits the camera to the now-relevant content — the same zoom-out-to-fit
// motion as the auto-triggered expand fit, since this is exactly that
// same "make sure the relevant branch is actually in view" moment.
function enterFocusMode(id) {
  state.focusedNodeId = id;
  // Focusing a branch without also selecting it left the reading pane and
  // the card's selected-highlight pointing at whatever was selected before
  // (or nothing) — setSelected's own render() covers the focus-mode change
  // too, so this doesn't need a separate render() call of its own.
  setSelected(id);
  fitToNode(id, MAX_ZOOM);
}

function exitFocusMode() {
  if (!state.focusedNodeId) {
    return;
  }
  state.focusedNodeId = null;
  render();
  fitToVisible();
}

// Shows (or hides) the small banner that is the normal view's only other
// cue of focus mode besides the blur itself — without it, someone who
// entered focus mode from a card two clicks ago (rather than clicking it
// just now) would have no way to tell the rest of the tree still exists.
function updateFocusModeBanner() {
  const banner = document.getElementById("focus-mode-banner");
  if (!state.focusedNodeId) {
    banner.hidden = true;
    return;
  }
  const title = summaryForNode(state.focusedNodeId)?.title ?? "";
  document.getElementById("focus-mode-banner-title").textContent = title;
  banner.hidden = false;
}

// Reloads a stale node's data and clears its stale mark, transitioning
// through a distinct loading state without resizing the node. Available
// from the node's own reload control or the `R` key on a stale selection.
// Reloading a node also reloads its entire cached subtree, not just itself:
// a reconnect gap marks every already-loaded node stale (see the workspace
// change handler), and a node's own re-fetch only refreshes its *immediate*
// children's summaries, not any grandchild that was separately fetched by
// expanding further down — those would otherwise stay stale until each is
// reloaded one at a time. Only descendants already cached (i.e. individually
// fetched before, whether currently expanded or not) are included; anything
// never fetched has nothing stale to refresh and will load fresh on demand.
function collectCachedDescendantIds(id, into) {
  into.push(id);
  const node = state.nodes.get(id);
  if (!node || !Array.isArray(node.children)) {
    return;
  }
  for (const child of node.children) {
    if (state.nodes.has(child.id)) {
      collectCachedDescendantIds(child.id, into);
    }
  }
}

async function reloadNode(id) {
  const ids = [];
  collectCachedDescendantIds(id, ids);
  for (const nodeId of ids) {
    state.loading.add(nodeId);
  }
  render();
  try {
    await Promise.all(ids.map((nodeId) => loadNode(activeWorkspaceId, nodeId)));
    for (const nodeId of ids) {
      state.stale.delete(nodeId);
    }
    syncSummaryInParent(id);
  } finally {
    for (const nodeId of ids) {
      state.loading.delete(nodeId);
    }
    render();
  }
}

// A node's own reload only refetches its own record, not how its parent's
// already-cached children array summarizes it (title, children_count,
// version) — patch that entry in place so a rename or version bump is
// visible in the tree immediately, not just once the parent happens to be
// refetched for some other reason.
function syncSummaryInParent(id) {
  const parentId = state.parents.get(id);
  const parent = parentId ? state.nodes.get(parentId) : null;
  const node = state.nodes.get(id);
  if (!parent || !node || !Array.isArray(parent.children)) {
    return;
  }
  const index = parent.children.findIndex((child) => child.id === id);
  if (index !== -1) {
    parent.children[index] = {
      id: node.id,
      slug: node.slug,
      title: node.title,
      children_count: node.children_count,
      version: node.version,
    };
  }
}

// Expands every node in the given node's subtree, fetching any not yet
// loaded. Recursion depth is bounded by the tree itself; a very large or
// deep subtree is a large-tree-handling concern addressed separately.
// Bumped on every new expand-all request; an in-flight traversal checks
// this after each await and stops as soon as it no longer matches, which is
// how both cancellation (Escape) and superseding-by-a-second-request work.
let expandGeneration = 0;
// How many nodes a staged expand-all processes before yielding a repaint
// and a turn of the event loop back to the browser — keeps a large subtree
// expansion responsive and its progress visible instead of freezing the
// page until the whole thing finishes.
const EXPAND_ALL_BATCH_SIZE = 25;

async function expandSubtree(id) {
  const generation = (expandGeneration += 1);
  state.expanding = true;
  render();
  try {
    await expandSubtreeStaged(id, generation, { processed: 0 });
  } finally {
    if (generation === expandGeneration) {
      state.expanding = false;
      render();
    }
  }
}

function cancelExpandAll() {
  if (!state.expanding) {
    return;
  }
  expandGeneration += 1;
  state.expanding = false;
  render();
}

async function expandSubtreeStaged(id, generation, progress) {
  if (generation !== expandGeneration || state.stale.has(id)) {
    return;
  }
  state.expanded.add(id);
  let node = state.nodes.get(id);
  if (!node?.children) {
    node = await loadNode(activeWorkspaceId, id);
    if (generation !== expandGeneration) {
      return;
    }
  }
  progress.processed += 1;
  if (progress.processed % EXPAND_ALL_BATCH_SIZE === 0) {
    render();
    await new Promise((resolve) => requestAnimationFrame(resolve));
    if (generation !== expandGeneration) {
      return;
    }
  }
  for (const child of node.children) {
    await expandSubtreeStaged(child.id, generation, progress);
    if (generation !== expandGeneration) {
      return;
    }
  }
}

function collapseSubtree(id) {
  state.expanded.delete(id);
  const node = state.nodes.get(id);
  for (const child of node?.children ?? []) {
    collapseSubtree(child.id);
  }
}

function setSelected(id) {
  // A visible node's summary (id/title/children_count) is available from its
  // parent's children array even before its own data has been fetched, so
  // selection does not require the fuller per-node fetch that expansion does.
  if (!id) {
    return;
  }
  state.selected = id;
  render();
  // The reading pane always mirrors the current selection now (rather than
  // requiring an explicit "open" action), so walking the tree with the
  // arrow keys shows each node's Markdown as you go — fire-and-forget, same
  // as every other async action triggered off a synchronous UI event here.
  updateReadingPane(id).catch(reportError);
}

// Keeps the docked reading pane showing whichever node is selected. Skips
// the fetch entirely while the pane is hidden or the node is marked stale
// (its content isn't trustworthy until reloaded); either way the title and
// stale banner still update, so switching selection or revealing a hidden
// pane never shows stale leftovers from a previous node.
async function updateReadingPane(id) {
  state.viewerNodeId = id;
  document.getElementById("reading-pane-title").textContent = summaryForNode(id)?.title ?? "";
  updateViewerStaleBanner();
  updateEditButtonVisibility();
  if (document.getElementById("reading-pane").hidden || state.stale.has(id)) {
    return;
  }
  const workspaceId = activeWorkspaceId;
  const rendered = await fetchJson(`/api/${workspaceId}/node/${encodeURIComponent(id)}/render`);
  // Superseded by a newer selection or a workspace switch while this was
  // in flight — the newer request (or updateReadingPane call) owns the
  // pane now, so applying this response would show the wrong node.
  if (state.selected !== id || activeWorkspaceId !== workspaceId) {
    return;
  }
  renderReadingPaneContent(rendered.html, { resetScroll: true });
}

function summaryForNode(id) {
  const loaded = state.nodes.get(id);
  if (loaded) {
    return loaded;
  }
  const parentId = state.parents.get(id);
  return state.nodes.get(parentId)?.children?.find((child) => child.id === id) ?? null;
}

// Toolbar buttons kept from EasyMDE's default set — no image-upload button
// (this deployment has no upload endpoint; the plain "image" action just
// inserts a `![]()` placeholder, which needs no backend) and no "guide"
// button (it link out to an external Markdown syntax reference, at odds
// with this app's otherwise fully offline, self-contained operation).
const EDITOR_TOOLBAR = [
  "bold", "italic", "heading", "|",
  "quote", "unordered-list", "ordered-list", "|",
  "link", "image", "code", "|",
  "preview", "side-by-side", "fullscreen",
];

function createMarkdownEditor(textarea, initialValue) {
  return new EasyMDE({
    element: textarea,
    initialValue,
    autofocus: true,
    spellChecker: false,
    status: false,
    toolbar: EDITOR_TOOLBAR,
    minHeight: "0",
  });
}

// Keeps the reading pane's Edit button in sync with whatever else is going
// on: hidden with nothing selected, while already editing/creating, or while
// the selection is stale (its cached content isn't trustworthy to seed an
// editor from until reloaded).
function updateEditButtonVisibility() {
  const id = state.selected;
  document.getElementById("reading-pane-edit").hidden =
    !id || Boolean(state.editing) || Boolean(state.creating) || state.stale.has(id);
}

function enterEditMode() {
  const id = state.selected;
  if (!id || state.editing || state.creating) {
    return;
  }
  const workspaceId = activeWorkspaceId;
  fetchJson(`/api/${workspaceId}/node/${encodeURIComponent(id)}/source`)
    .then((source) => {
      // Superseded by a different selection or a workspace switch while the
      // fetch was in flight.
      if (state.selected !== id || activeWorkspaceId !== workspaceId) {
        return;
      }
      state.editing = { id, expectedVersion: source.version };
      document.getElementById("viewer-content").classList.add("hidden");
      document.getElementById("viewer-stale-banner").hidden = true;
      document.getElementById("viewer-editor").classList.remove("hidden");
      document.getElementById("viewer-editor").classList.add("flex");
      updateEditButtonVisibility();
      const conflict = document.getElementById("viewer-editor-conflict");
      conflict.hidden = true;
      conflict.textContent = "";
      const textarea = document.getElementById("viewer-editor-textarea");
      easyMdeInstance = createMarkdownEditor(textarea, source.markdown_content);
    })
    .catch(reportError);
}

function exitEditMode() {
  if (!state.editing) {
    return;
  }
  if (easyMdeInstance) {
    easyMdeInstance.toTextArea();
    easyMdeInstance = null;
  }
  state.editing = null;
  document.getElementById("viewer-editor").classList.add("hidden");
  document.getElementById("viewer-editor").classList.remove("flex");
  document.getElementById("viewer-content").classList.remove("hidden");
  updateEditButtonVisibility();
  updateViewerStaleBanner();
}

function setEditorSaving(saving) {
  const button = document.getElementById("viewer-editor-save");
  button.disabled = saving;
  button.textContent = saving ? "Saving…" : "Save";
  document.getElementById("viewer-editor-cancel").disabled = saving;
}

function showEditConflict(message) {
  const conflict = document.getElementById("viewer-editor-conflict");
  conflict.textContent = message;
  conflict.hidden = false;
}

function saveEdit() {
  if (!state.editing || state.pendingEdit) {
    return;
  }
  const { id, expectedVersion } = state.editing;
  const content = easyMdeInstance.value();
  state.pendingEdit = { id, expectedVersion };
  setEditorSaving(true);
  const sent = sendCommand("update_node", {
    selector: id,
    content,
    expected_version: expectedVersion,
  });
  if (!sent) {
    state.pendingEdit = null;
    setEditorSaving(false);
    showEditConflict("Not connected to the server — try again once reconnected.");
  }
}

// Deliberately does not mark the node `stale` on reject, unlike the drag
// gestures (see handleDragCommandResponse): Save is a conscious foreground
// action the user is actively watching, so a version conflict is surfaced
// right in the editor instead, with the option to keep editing or reload.
async function handleUpdateNodeResponse(envelope) {
  const pending = state.pendingEdit;
  state.pendingEdit = null;
  setEditorSaving(false);
  if (envelope.type !== "ack") {
    showEditConflict(
      envelope.payload?.reason ?? "Save failed: the node changed since editing began.",
    );
    return;
  }
  noteSelfCausedChange();
  state.editing = null;
  if (easyMdeInstance) {
    easyMdeInstance.toTextArea();
    easyMdeInstance = null;
  }
  document.getElementById("viewer-editor").classList.add("hidden");
  document.getElementById("viewer-editor").classList.remove("flex");
  document.getElementById("viewer-content").classList.remove("hidden");
  // Left non-stale, unlike a create/reorder/move's affected parent: this is
  // the node the user was just directly looking at and saving, so showing
  // it dashed-and-muted right after Save would read as broken, not as a
  // useful "something changed" signal.
  state.stale.delete(pending.id);
  await loadNode(activeWorkspaceId, pending.id);
  const parentId = state.parents.get(pending.id);
  if (parentId) {
    await loadNode(activeWorkspaceId, parentId);
  }
  updateEditButtonVisibility();
  if (state.viewerNodeId === pending.id) {
    // Known-authoritative content this same save just wrote — fetch the
    // rendered view directly rather than through updateReadingPane's normal
    // staleness-skip guard.
    const rendered = await fetchJson(
      `/api/${activeWorkspaceId}/node/${encodeURIComponent(pending.id)}/render`,
    );
    if (state.viewerNodeId === pending.id) {
      renderReadingPaneContent(rendered.html);
    }
  }
  render();
}

function cleanupCreateForm() {
  if (easyMdeInstance) {
    easyMdeInstance.toTextArea();
    easyMdeInstance = null;
  }
  state.creating = null;
  document.getElementById("viewer-create").classList.add("hidden");
  document.getElementById("viewer-create").classList.remove("flex");
}

function openCreateChildForm(parentId) {
  if (state.creating || state.pendingCreate) {
    return;
  }
  exitEditMode();
  state.creating = { parentId };
  setReadingPaneHidden(false);
  document.getElementById("viewer-content").classList.add("hidden");
  document.getElementById("viewer-stale-banner").hidden = true;
  updateEditButtonVisibility();
  const parentTitle = summaryForNode(parentId)?.title;
  document.getElementById("reading-pane-title").textContent = parentTitle
    ? `New node under ${parentTitle}`
    : "New node";
  const error = document.getElementById("viewer-create-error");
  error.hidden = true;
  error.textContent = "";
  const titleInput = document.getElementById("viewer-create-title");
  titleInput.value = "";
  document.getElementById("viewer-create-slug").value = "";
  const panel = document.getElementById("viewer-create");
  panel.classList.remove("hidden");
  panel.classList.add("flex");
  const textarea = document.getElementById("viewer-create-textarea");
  easyMdeInstance = createMarkdownEditor(textarea, "");
  titleInput.focus();
}

function closeCreateChildForm() {
  if (!state.creating) {
    return;
  }
  cleanupCreateForm();
  document.getElementById("viewer-content").classList.remove("hidden");
  if (state.selected) {
    updateReadingPane(state.selected).catch(reportError);
  } else {
    document.getElementById("reading-pane-title").textContent = "";
    updateEditButtonVisibility();
  }
}

function setCreateSaving(saving) {
  const button = document.getElementById("viewer-create-submit");
  button.disabled = saving;
  button.textContent = saving ? "Creating…" : "Create";
  document.getElementById("viewer-create-cancel").disabled = saving;
}

function showCreateError(message) {
  const error = document.getElementById("viewer-create-error");
  error.textContent = message;
  error.hidden = false;
}

// Mirrors mdtree_core::Slug's own validation (lowercase ASCII letters,
// digits, and single interior hyphens) so an obviously invalid slug fails
// fast, without a round trip — the server re-validates it regardless, since
// this check exists purely for faster feedback, not as the source of truth.
const SLUG_PATTERN = /^[a-z0-9]+(-[a-z0-9]+)*$/;

function submitCreateChild() {
  if (!state.creating || state.pendingCreate) {
    return;
  }
  const title = document.getElementById("viewer-create-title").value.trim();
  if (!title) {
    showCreateError("Title is required.");
    return;
  }
  const slug = document.getElementById("viewer-create-slug").value.trim();
  if (slug && !SLUG_PATTERN.test(slug)) {
    showCreateError(
      "Slug must be lowercase letters, digits, and single hyphens only (e.g. \"database-models\").",
    );
    return;
  }
  const content = easyMdeInstance.value();
  const parentId = state.creating.parentId;
  state.pendingCreate = { parentId };
  setCreateSaving(true);
  const sent = sendCommand("create_node", {
    parent: parentId,
    title,
    ...(slug ? { slug } : {}),
    // An untouched body is omitted so the server's own `# {title}` default
    // (rather than a client-guessed duplicate) is authoritative.
    ...(content.trim() ? { content } : {}),
  });
  if (!sent) {
    state.pendingCreate = null;
    setCreateSaving(false);
    showCreateError("Not connected to the server — try again once reconnected.");
  }
}

// Same reasoning as handleUpdateNodeResponse: a rejected create surfaces
// inline in the still-open form rather than as silent staleness.
async function handleCreateNodeResponse(envelope) {
  const pending = state.pendingCreate;
  state.pendingCreate = null;
  setCreateSaving(false);
  if (envelope.type !== "ack") {
    showCreateError(envelope.payload?.reason ?? "Create failed.");
    return;
  }
  noteSelfCausedChange();
  const newId = envelope.payload.node_id;
  cleanupCreateForm();
  document.getElementById("viewer-content").classList.remove("hidden");
  state.expanded.add(pending.parentId);
  await loadNode(activeWorkspaceId, pending.parentId);
  // Flagged stale on purpose, even though it was just refreshed above: a
  // visible "this node just gained a child" signal on the parent itself,
  // per the requirement that this show up there — not on the whole tree
  // (the previous behavior, before this and the suppression below) and not
  // silently invisible (simply staying non-stale would be, otherwise).
  state.stale.add(pending.parentId);
  setSelected(newId);
  render();
  centerOnNode(newId);
}

// True if `candidateId` is `targetId` itself or nested somewhere below it —
// used by handleRemoveNodeResponse to tell whether the just-deleted subtree
// took the current selection or open viewer down with it, so either can be
// redirected to the parent instead of dangling on a node that no longer
// exists.
function isNodeOrDescendant(candidateId, targetId) {
  let current = candidateId;
  while (current !== undefined) {
    if (current === targetId) {
      return true;
    }
    current = state.parents.get(current);
  }
  return false;
}

// Id of the node the `#delete-confirm-overlay` dialog is currently asking
// about — set when it opens, read by its own Confirm button, cleared on
// close. Same single-shared-dialog pattern as `nodeCardMenuTargetId`.
let deleteConfirmTargetId = null;

// Opens the styled confirmation dialog (in place of a native
// `window.confirm`, which can't carry the app's own look) before ever
// sending `remove_node`; dismissing it sends nothing. The root is never
// offered this action in the first place (see showNodeCardMenu), so it
// never reaches here.
function requestDeleteNode(id) {
  if (state.pendingDelete) {
    return;
  }
  const summary = summaryForNode(id);
  if (!summary || summary.version === undefined) {
    return;
  }
  deleteConfirmTargetId = id;
  const childCount = summary.children_count ?? 0;
  const subtreeWarning = childCount
    ? ` and its ${childCount} child node${childCount === 1 ? "" : "s"}`
    : "";
  document.getElementById("delete-confirm-message").textContent =
    `Delete "${summary.title}"${subtreeWarning}? This cannot be undone.`;
  const overlay = document.getElementById("delete-confirm-overlay");
  overlay.hidden = false;
  // Cancel, not Delete, is the default focus target — an errant Enter
  // press (e.g. right after dismissing some other focused control) should
  // never itself confirm a destructive action.
  document.getElementById("delete-confirm-cancel").focus();
}

function hideDeleteConfirm() {
  document.getElementById("delete-confirm-overlay").hidden = true;
  deleteConfirmTargetId = null;
}

// Runs only once the styled dialog's own Confirm button is clicked — never
// called directly from requestDeleteNode.
function confirmDeleteNode() {
  const id = deleteConfirmTargetId;
  hideDeleteConfirm();
  if (!id || state.pendingDelete) {
    return;
  }
  const summary = summaryForNode(id);
  const parentId = state.parents.get(id);
  if (!summary || summary.version === undefined || !parentId) {
    return;
  }
  state.pendingDelete = { id, parentId };
  state.loading.add(id);
  render();
  const sent = sendCommand("remove_node", {
    selector: id,
    expected_version: summary.version,
  });
  if (!sent) {
    state.loading.delete(id);
    state.pendingDelete = null;
    render();
  }
}

// Mirrors handleCreateNodeResponse's "refetch the affected parent" idiom;
// unlike create, a rejected delete has no open form to surface the reason
// inline in, so it just logs (see reportError) and leaves the node as it
// was.
async function handleRemoveNodeResponse(envelope) {
  const pending = state.pendingDelete;
  state.pendingDelete = null;
  state.loading.delete(pending.id);
  if (envelope.type !== "ack") {
    reportError(new Error(envelope.payload?.reason ?? "Delete failed."));
    render();
    return;
  }
  noteSelfCausedChange();
  const { id, parentId } = pending;
  const needsReselect =
    (state.selected !== null && isNodeOrDescendant(state.selected, id)) ||
    (state.viewerNodeId !== null && isNodeOrDescendant(state.viewerNodeId, id));
  await loadNode(activeWorkspaceId, parentId);
  state.stale.add(parentId);
  if (needsReselect) {
    setSelected(parentId);
    centerOnNode(parentId);
  } else {
    render();
  }
}

// Registers that our own just-acked create/update/reorder/move already
// applied its own precise `state.stale` adjustment above — so the "change"
// broadcast that same mutation triggers (always arriving after this ack,
// over this same connection; see change_hub.rs) skips the usual "can't tell
// what changed, mark everything loaded stale" fallback entirely for that
// one event, rather than immediately overwriting the adjustment just made.
function noteSelfCausedChange() {
  const workspace = workspaces.get(activeWorkspaceId);
  if (workspace) {
    workspace.suppressNextChangeSweeps += 1;
  }
}

async function copySelectedPath() {
  const selectedId = state.selected;
  if (!selectedId) {
    return;
  }
  const path = summaryForNode(selectedId)?.path;
  if (!path) {
    return;
  }
  await writeClipboard(path);
  flashCopiedNode(selectedId);
}

function flashCopiedNode(id) {
  const card = Array.from(document.querySelectorAll(".node-card")).find(
    (candidate) => candidate.dataset.id === id,
  );
  if (!card) {
    return;
  }

  // Removing the class and flushing layout lets repeated C presses restart
  // the pulse instead of being swallowed by an animation already in flight.
  card.classList.remove("path-copied");
  void card.offsetWidth;
  card.classList.add("path-copied");
  const finish = (event) => {
    if (
      event.target !== card ||
      !event.animationName.startsWith("node-path-copied")
    ) {
      return;
    }
    card.classList.remove("path-copied");
    card.removeEventListener("animationend", finish);
  };
  card.addEventListener("animationend", finish);
}

// Moves selection through the currently visible hierarchy: up to the parent,
// down to the first visible child, left/right between siblings in sibling
// order. Does not open the Markdown viewer — that is a distinct action.
function moveSelection(direction) {
  if (!state.selected) {
    return;
  }
  const current = state.selected;

  if (direction === "parent") {
    const parentId = state.parents.get(current);
    if (parentId) {
      selectAndReveal(parentId);
    }
    return;
  }

  if (direction === "child") {
    const node = state.nodes.get(current);
    if (state.expanded.has(current) && node?.children?.length) {
      selectAndReveal(node.children[0].id);
    } else if (summaryForNode(current)?.children_count > 0) {
      // Closed but has children: open it first (same as Space/Enter/a
      // double-click) rather than doing nothing — a second ArrowRight then
      // steps into the now-visible first child via the branch above.
      toggleExpand(current).catch(reportError);
    }
    return;
  }

  const parentId = state.parents.get(current);
  const siblings = parentId ? state.nodes.get(parentId)?.children : null;
  if (!siblings) {
    return;
  }
  const index = siblings.findIndex((child) => child.id === current);
  if (index === -1) {
    return;
  }
  const nextIndex = direction === "previous-sibling" ? index - 1 : index + 1;
  if (nextIndex >= 0 && nextIndex < siblings.length) {
    selectAndReveal(siblings[nextIndex].id);
  }
}

// Selects `id` and, only if that just carried the selection outside the
// canvas viewport, pans the camera the minimum amount needed to bring its
// card back on screen — in whichever direction (up/down/left/right) it went
// off. Unlike `centerOnNode`/`fitToNode` (used for search jumps and
// expand/collapse), this never recenters or rescales a node that's already
// visible, so plain arrow-key steps within view don't jump the camera at
// every keypress.
function selectAndReveal(id) {
  setSelected(id);
  panIntoViewIfNeeded(id);
}

// Padding kept between a just-revealed card and the canvas edge, so it isn't
// flush against the border immediately after panning.
const VIEWPORT_REVEAL_MARGIN = 24;

function panIntoViewIfNeeded(id) {
  const position = state.positions.get(id);
  if (!position) {
    return;
  }
  const canvas = document.getElementById("tree-canvas");
  const rect = canvas.getBoundingClientRect();
  const scale = state.camera.scale || 1;
  const height = cardHeightFor(position.node);
  const screenLeft = state.camera.x + (position.x - CARD_WIDTH / 2) * scale;
  const screenRight = state.camera.x + (position.x + CARD_WIDTH / 2) * scale;
  const screenTop = state.camera.y + (position.y - height / 2) * scale;
  const screenBottom = state.camera.y + (position.y + height / 2) * scale;

  let dx = 0;
  if (screenLeft < 0) {
    dx = VIEWPORT_REVEAL_MARGIN - screenLeft;
  } else if (screenRight > rect.width) {
    dx = rect.width - VIEWPORT_REVEAL_MARGIN - screenRight;
  }
  let dy = 0;
  if (screenTop < 0) {
    dy = VIEWPORT_REVEAL_MARGIN - screenTop;
  } else if (screenBottom > rect.height) {
    dy = rect.height - VIEWPORT_REVEAL_MARGIN - screenBottom;
  }
  if (dx === 0 && dy === 0) {
    return;
  }
  state.camera.x += dx;
  state.camera.y += dy;
  applyCameraAnimated();
}

function renderReadingPaneContent(html, { resetScroll = false } = {}) {
  const content = document.getElementById("viewer-content");
  content.innerHTML = html;
  addCodeCopyButtons(content);
  if (resetScroll) {
    content.scrollTop = 0;
  }
}

// Adds a compact copy control to every fenced code block, positioned so it
// never covers the code itself. The button is client-side chrome added
// after sanitized content is inserted, not part of the untrusted HTML.
function addCodeCopyButtons(container) {
  for (const pre of container.querySelectorAll("pre")) {
    const source = (pre.querySelector("code") ?? pre).textContent;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "copy-code";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy code block");
    button.addEventListener("click", () => copyCode(button, source));
    pre.appendChild(button);
  }
}

async function copyCode(button, text) {
  try {
    await writeClipboard(text);
  } catch (error) {
    reportError(error);
    return;
  }
  const original = button.textContent;
  button.textContent = "Copied";
  button.classList.add("copied");
  setTimeout(() => {
    button.textContent = original;
    button.classList.remove("copied");
  }, 1500);
}

async function writeClipboard(text) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }

  // Clipboard API access is restricted to secure contexts in some browsers.
  // Remote trusted-network sessions may be plain HTTP, so retain a synchronous
  // selection-based fallback that still runs inside the keyboard/click gesture.
  const source = document.createElement("textarea");
  source.value = text;
  source.setAttribute("readonly", "");
  source.style.position = "fixed";
  source.style.opacity = "0";
  document.body.appendChild(source);
  source.select();
  const copied = document.execCommand("copy");
  source.remove();
  if (!copied) {
    throw new Error("clipboard copy is unavailable");
  }
}

// Reflects whether the node currently displayed in the viewer has gone
// stale since it was opened (e.g. a live "change" notification arrived
// while reading it). The pane keeps showing the already-loaded snapshot;
// this only surfaces a non-intrusive banner and reload action, never an
// automatic content replacement.
function updateViewerStaleBanner() {
  document.getElementById("viewer-stale-banner").hidden = !state.stale.has(state.viewerNodeId);
}

// Shows or hides the docked reading pane, persisting the choice like the
// other panel toggles here so it survives a page reload. Revealing it
// refreshes its content for whatever is currently selected, since it skips
// fetching entirely while hidden (see updateReadingPane).
const READING_PANE_HIDDEN_KEY = "mdtree-reading-pane-hidden";

function setReadingPaneHidden(hidden) {
  const pane = document.getElementById("reading-pane");
  pane.hidden = hidden;
  localStorage.setItem(READING_PANE_HIDDEN_KEY, hidden ? "true" : "false");
  if (!hidden && state.selected) {
    updateReadingPane(state.selected).catch(reportError);
  }
}

function toggleReadingPane() {
  setReadingPaneHidden(!document.getElementById("reading-pane").hidden);
}

// Drag-to-resize via a thin handle overlapping the pane's left border, and
// a maximize toggle that hides the canvas entirely so the pane fills all
// the way to the left panel. Width (but not the maximized state, which is
// meant as a temporary full-width look rather than a lasting preference)
// persists across reloads like the other panel toggles here.
const READING_PANE_WIDTH_KEY = "mdtree-reading-pane-width";
const READING_PANE_MIN_WIDTH = 280;
const READING_PANE_MAX_WIDTH_RATIO = 0.8;
const READING_PANE_DEFAULT_WIDTH = 416; // 26rem, matching the pane's original fixed width

function setUpReadingPaneResize() {
  const pane = document.getElementById("reading-pane");
  const handle = document.getElementById("reading-pane-resize-handle");
  const savedWidth = Number(localStorage.getItem(READING_PANE_WIDTH_KEY));
  pane.style.width = `${savedWidth > 0 ? savedWidth : READING_PANE_DEFAULT_WIDTH}px`;

  let dragging = false;
  handle.addEventListener("pointerdown", (event) => {
    if (isReadingPaneMaximized()) {
      return; // resizing a maximized (canvas-hidden) pane makes no sense
    }
    dragging = true;
    handle.setPointerCapture(event.pointerId);
    event.preventDefault();
  });
  handle.addEventListener("pointermove", (event) => {
    if (!dragging) {
      return;
    }
    const maxWidth = window.innerWidth * READING_PANE_MAX_WIDTH_RATIO;
    const width = clamp(window.innerWidth - event.clientX, READING_PANE_MIN_WIDTH, maxWidth);
    pane.style.width = `${width}px`;
  });
  const stopDragging = () => {
    if (!dragging) {
      return;
    }
    dragging = false;
    localStorage.setItem(READING_PANE_WIDTH_KEY, String(Math.round(pane.getBoundingClientRect().width)));
  };
  handle.addEventListener("pointerup", stopDragging);
  handle.addEventListener("pointercancel", stopDragging);
}

function isReadingPaneMaximized() {
  return document.getElementById("tree-canvas").hidden;
}

function toggleReadingPaneMaximized() {
  const pane = document.getElementById("reading-pane");
  const canvas = document.getElementById("tree-canvas");
  const maximizing = !isReadingPaneMaximized();
  canvas.hidden = maximizing;
  // `flex-1` overrides the pane's own explicit pixel width to fill the row
  // now that the canvas beside it is gone; toggling back to `flex-none`
  // lets that same width (untouched throughout) take over again.
  pane.classList.toggle("flex-none", !maximizing);
  pane.classList.toggle("flex-1", maximizing);
  document.getElementById("reading-pane-resize-handle").hidden = maximizing;
  const button = document.getElementById("reading-pane-maximize");
  button.title = maximizing ? "Restore preview width" : "Maximize preview";
  button.setAttribute("aria-label", button.title);
  const iconPath = maximizing
    ? '<path d="M7 5l5 5-5 5" stroke-linecap="round" stroke-linejoin="round" /><path d="M12 5l5 5-5 5" stroke-linecap="round" stroke-linejoin="round" />'
    : '<path d="M13 5l-5 5 5 5" stroke-linecap="round" stroke-linejoin="round" /><path d="M8 5l-5 5 5 5" stroke-linecap="round" stroke-linejoin="round" />';
  button.innerHTML = `<svg viewBox="0 0 20 20" fill="none" class="h-4 w-4" stroke="currentColor" stroke-width="1.6">${iconPath}</svg>`;
}

function toggleShortcutHelp() {
  const help = document.getElementById("shortcut-help");
  help.hidden = !help.hidden;
}

function hideShortcutHelp() {
  document.getElementById("shortcut-help").hidden = true;
}

function reportError(error) {
  // eslint-disable-next-line no-console
  console.error("mdtree browse-ui:", error);
}

// Matches the tree's actual left-to-right layout (root pinned at the
// top-left, each generation of children cascading to its right, siblings
// stacked top to bottom): Left/Right cross generations toward the parent or
// first child, since that's the direction they're actually drawn in; Up/Down
// stay within the same generation, moving between siblings stacked above or
// below.
const ARROW_DIRECTIONS = {
  ArrowLeft: "parent",
  ArrowRight: "child",
  ArrowUp: "previous-sibling",
  ArrowDown: "next-sibling",
};

// Z is a chord key for zoom (hold Z, then press + or -), not a shortcut on
// its own — tracked here (rather than checked via a modifier flag, since
// KeyboardEvent has no generic "is this arbitrary key currently held" query
// the way it does for Ctrl/Shift/Alt/Meta) so Z+Plus/Z+Minus only fire while
// Z is actually still held down, not just pressed at some earlier point.
let zKeyHeld = false;
document.addEventListener("keyup", (event) => {
  if (event.key === "z" || event.key === "Z") {
    zKeyHeld = false;
  }
});
// Losing focus (e.g. alt-tabbing away mid-chord) never fires a keyup for
// whatever was held — drop the chord state so it can't get stuck "on".
window.addEventListener("blur", () => {
  zKeyHeld = false;
});

document.addEventListener("keydown", (event) => {
  // Every shortcut below is a single bare letter/key with no modifier
  // besides Shift — exactly the keys someone typing a search query needs to
  // type freely. Bail out whenever a text field has focus (the search
  // input handles its own Escape/Enter locally) rather than hijacking
  // keystrokes meant for it.
  const focused = document.activeElement;
  if (focused instanceof HTMLElement && (focused.tagName === "INPUT" || focused.tagName === "TEXTAREA" || focused.isContentEditable)) {
    return;
  }
  // Preserve standard browser shortcuts such as copy, paste, find, and
  // reload. Without this guard, Ctrl/Cmd+C is mistaken for the app's bare C
  // shortcut below and preventDefault() suppresses copying selected Markdown.
  if (event.ctrlKey || event.metaKey) {
    return;
  }
  if (event.key === "Escape") {
    if (!document.getElementById("delete-confirm-overlay").hidden) {
      hideDeleteConfirm();
    } else if (!document.getElementById("node-card-menu").hidden) {
      hideNodeCardMenu();
    } else if (openRootCurrentMenus.some(({ menu }) => !menu.hidden)) {
      hideOpenRootCurrentMenu();
    } else if (state.expanding) {
      cancelExpandAll();
    } else if (!document.getElementById("shortcut-help").hidden) {
      hideShortcutHelp();
    } else if (state.focusedNodeId) {
      exitFocusMode();
    } else {
      setReadingPaneHidden(true);
    }
    return;
  }
  if (event.key === "?") {
    event.preventDefault();
    toggleShortcutHelp();
    return;
  }
  if (event.key === "Home") {
    event.preventDefault();
    setSelected(state.root);
    return;
  }
  if (event.key === "/") {
    event.preventDefault();
    document.getElementById("control-search").click();
    return;
  }
  if (event.shiftKey && event.key === "F") {
    // Same "zoom to max if collapsed, fit to bounds if expanded" behavior as
    // a search jump (see focusSearchResult) — for the branch you already
    // have selected, rather than one just found via search.
    event.preventDefault();
    if (state.selected) {
      fitToNode(state.selected, MAX_ZOOM);
    }
    return;
  }
  if (event.key === "f" || event.key === "F") {
    event.preventDefault();
    fitToView();
    return;
  }
  if (event.key === "0") {
    event.preventDefault();
    resetZoom();
    return;
  }
  if (event.key === "z" || event.key === "Z") {
    zKeyHeld = true;
    return;
  }
  if (zKeyHeld && event.key === "+") {
    event.preventDefault();
    zoomIn();
    return;
  }
  if (zKeyHeld && event.key === "-") {
    event.preventDefault();
    zoomOut();
    return;
  }
  if (event.key === " ") {
    event.preventDefault();
    if (state.selected) {
      toggleExpand(state.selected).catch(reportError);
    }
    return;
  }
  if (event.key === "v" || event.key === "V") {
    event.preventDefault();
    toggleReadingPane();
    return;
  }
  if (event.key === "Enter") {
    event.preventDefault();
    if (state.selected) {
      toggleExpand(state.selected).catch(reportError);
    }
    return;
  }
  if (event.altKey && (event.key === "e" || event.key === "E")) {
    event.preventDefault();
    expandRoot();
    return;
  }
  if (event.altKey && (event.key === "c" || event.key === "C")) {
    event.preventDefault();
    collapseRoot();
    return;
  }
  if (event.shiftKey && event.key === "E") {
    event.preventDefault();
    expandSelected();
    return;
  }
  if (event.shiftKey && event.key === "C") {
    event.preventDefault();
    collapseSelected();
    return;
  }
  if (!event.shiftKey && (event.key === "c" || event.key === "C")) {
    event.preventDefault();
    copySelectedPath().catch(reportError);
    return;
  }
  if (event.key === "r" || event.key === "R") {
    event.preventDefault();
    if (state.selected && state.stale.has(state.selected)) {
      reloadNode(state.selected).catch(reportError);
    }
    return;
  }
  if (event.key === "t" || event.key === "T") {
    event.preventDefault();
    toggleTheme();
    return;
  }
  const direction = ARROW_DIRECTIONS[event.key];
  if (direction) {
    event.preventDefault();
    moveSelection(direction);
  }
});

document.getElementById("shortcut-help").addEventListener("click", (event) => {
  if (event.target.id === "shortcut-help") {
    hideShortcutHelp();
  }
});

document.getElementById("reading-pane-hide").addEventListener("click", () => {
  setReadingPaneHidden(true);
});

document.getElementById("reading-pane-maximize").addEventListener("click", () => {
  toggleReadingPaneMaximized();
});

document.getElementById("reading-pane-edit").addEventListener("click", () => {
  enterEditMode();
});

document.getElementById("viewer-editor-cancel").addEventListener("click", () => {
  exitEditMode();
});

document.getElementById("viewer-editor-save").addEventListener("click", () => {
  saveEdit();
});

document.getElementById("viewer-create-cancel").addEventListener("click", () => {
  closeCreateChildForm();
});

document.getElementById("viewer-create-submit").addEventListener("click", () => {
  submitCreateChild();
});

document.getElementById("node-card-menu-focus").addEventListener("click", () => {
  const id = nodeCardMenuTargetId;
  hideNodeCardMenu();
  if (id) {
    enterFocusMode(id);
  }
});

document.getElementById("node-card-menu-add-child").addEventListener("click", () => {
  const id = nodeCardMenuTargetId;
  hideNodeCardMenu();
  if (id) {
    openCreateChildForm(id);
  }
});

document.getElementById("node-card-menu-delete").addEventListener("click", () => {
  const id = nodeCardMenuTargetId;
  hideNodeCardMenu();
  if (id) {
    requestDeleteNode(id);
  }
});

// This menu's three actions are icon-only (see index.html, matching the
// Fit/Expand/Collapse menus' own look), so each needs the shared hover
// tooltip wired by hand instead of relying on visible text.
document.getElementById("node-card-menu-focus").addEventListener("pointerenter", (event) => {
  showTooltip(event.currentTarget, "Show only this branch", "above");
});
document.getElementById("node-card-menu-focus").addEventListener("pointerleave", hideTooltip);
document.getElementById("node-card-menu-add-child").addEventListener("pointerenter", (event) => {
  showTooltip(event.currentTarget, "Add child node", "above");
});
document.getElementById("node-card-menu-add-child").addEventListener("pointerleave", hideTooltip);
document.getElementById("node-card-menu-delete").addEventListener("pointerenter", (event) => {
  showTooltip(event.currentTarget, "Delete node", "above");
});
document.getElementById("node-card-menu-delete").addEventListener("pointerleave", hideTooltip);

document.getElementById("delete-confirm-cancel").addEventListener("click", () => {
  hideDeleteConfirm();
});

document.getElementById("delete-confirm-confirm").addEventListener("click", () => {
  confirmDeleteNode();
});

// Clicking the dimmed backdrop is the same as Cancel — but only a click
// that both starts and ends on the backdrop itself counts, so dragging a
// text selection from inside the dialog out past its edge before
// releasing doesn't accidentally dismiss it.
document.getElementById("delete-confirm-overlay").addEventListener("mousedown", (event) => {
  if (event.target === event.currentTarget) {
    event.currentTarget.dataset.backdropArmed = "1";
  }
});
document.getElementById("delete-confirm-overlay").addEventListener("click", (event) => {
  if (event.target === event.currentTarget && event.currentTarget.dataset.backdropArmed === "1") {
    hideDeleteConfirm();
  }
  delete event.currentTarget.dataset.backdropArmed;
});

// Same "click outside closes it" pattern as the search popover and the
// root/current menus (see setUpRootCurrentMenu).
document.addEventListener("pointerdown", (event) => {
  const menu = document.getElementById("node-card-menu");
  if (!menu.hidden && !menu.contains(event.target) && !event.target.closest(".node-card-more")) {
    hideNodeCardMenu();
  }
});

document.getElementById("focus-mode-exit").addEventListener("click", () => {
  exitFocusMode();
});

document.getElementById("viewer-reload").addEventListener("click", async () => {
  const id = state.viewerNodeId;
  if (!id) {
    return;
  }
  try {
    await reloadNode(id);
    if (state.viewerNodeId === id && !state.stale.has(id)) {
      await updateReadingPane(id);
    }
  } catch (error) {
    reportError(error);
  }
});

init().catch(reportError);
