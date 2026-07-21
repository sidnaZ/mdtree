//! Stable command-line adapter for `MDTree` services.

use std::collections::BTreeMap;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::str::FromStr;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use mdtree_core::{
    BatchChildrenRequest, CloneSubtreeRequest, Node, NodeId, NodeMetadata, NodeSelector, NodeType,
    PageCursor, PageLimit, PaginationError, Reference, ReferenceOrigin, ReferenceTarget,
    ReferenceType, SearchFilters, SearchRequest, SearchScope, Slug, SystemUlidGenerator,
    UlidGenerator, DEFAULT_PAGE_LIMIT,
};
use mdtree_sqlite::{
    backup_workspace, check_workspace, doctor_workspace, export_markdown_node,
    export_markdown_snapshot, export_snapshot_json, import_markdown_snapshot_new,
    import_snapshot_new, plan_json_import, prepare_node_mutation, restore_workspace,
    workspace_status, AtomicTreeMove, AtomicTreeRemoval, CheckStatus, NodeMutationDraft,
    PreparedBatchOperation, PreparedNodeMutation, SqliteStore, WorkspaceError,
};
use serde::{Deserialize, Serialize};

/// Process exit code for successful requests.
pub const EXIT_OK: u8 = 0;
/// Process exit code for invalid workspace findings or user input.
pub const EXIT_INVALID: u8 = 2;
/// Process exit code for operational failures.
pub const EXIT_OPERATIONAL: u8 = 1;

/// Output encoding shared by every command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    /// Concise human-readable JSON representation.
    Text,
    /// One complete JSON value.
    Json,
    /// One JSON value per output item.
    Jsonl,
}

/// Structural relation selected by `navigate`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NavigationRelation {
    /// Direct canonical parent.
    Parent,
    /// Direct children in canonical sibling order.
    Children,
    /// Root-to-parent ancestor chain.
    Ancestors,
    /// Descendants in canonical depth-first order.
    Descendants,
    /// Nodes sharing the same canonical parent.
    Siblings,
    /// Selected node and descendants in canonical depth-first order.
    Subtree,
}

/// Structural child-existence filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum FilterPredicate {
    /// Nodes without canonical children.
    Leaf,
    /// Nodes with at least one canonical child.
    Internal,
}

/// Database-side traversal order for descendants and subtree enumeration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum TraversalOrder {
    /// Pre-order depth-first traversal.
    #[default]
    Dfs,
    /// Breadth-first traversal by relative depth.
    Bfs,
}

impl From<TraversalOrder> for mdtree_core::TraversalOrder {
    fn from(value: TraversalOrder) -> Self {
        match value {
            TraversalOrder::Dfs => Self::Dfs,
            TraversalOrder::Bfs => Self::Bfs,
        }
    }
}

impl From<FilterPredicate> for mdtree_core::StructuralPredicate {
    fn from(value: FilterPredicate) -> Self {
        match value {
            FilterPredicate::Leaf => Self::Leaf,
            FilterPredicate::Internal => Self::Internal,
        }
    }
}

/// Snapshot interchange encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SnapshotFormat {
    /// Versioned JSON file.
    Json,
    /// Versioned Markdown directory tree.
    Markdown,
}

/// Reusable CLI continuation flags for paginated collection commands.
#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct PaginationArgs {
    /// Maximum items in this page (1 through 100).
    #[arg(long, default_value_t = DEFAULT_PAGE_LIMIT)]
    pub limit: u32,
    /// Opaque continuation token returned by the preceding page.
    #[arg(long)]
    pub cursor: Option<String>,
}

impl PaginationArgs {
    /// Parses the shared limit and opaque cursor contracts.
    ///
    /// # Errors
    ///
    /// Returns the stable shared validation error for an invalid limit or cursor.
    pub fn validated(&self) -> Result<(PageLimit, Option<PageCursor>), PaginationError> {
        Ok((
            PageLimit::new(self.limit)?,
            self.cursor.as_deref().map(str::parse).transpose()?,
        ))
    }
}

/// Local-first Markdown tree manager.
#[derive(Debug, Parser)]
#[command(name = "mdtree", version, about)]
pub struct Cli {
    /// Workspace database path (or `MDTREE_WORKSPACE`; defaults to `.mdtree`).
    #[arg(long, global = true, env = "MDTREE_WORKSPACE")]
    pub workspace: Option<PathBuf>,
    /// Display name for the primary workspace in the web UI (defaults to a
    /// name derived from the workspace file, e.g. its containing directory).
    #[arg(long, global = true)]
    pub workspace_name: Option<String>,
    /// Stable output encoding.
    #[arg(long, global = true, value_enum, default_value = "text")]
    pub output: OutputFormat,
    /// Skip opening the default browser for a web UI launch.
    #[arg(long, global = true)]
    pub no_open: bool,
    /// TCP port for the web UI; omitted or 0 lets the operating system choose.
    #[arg(long, global = true, default_value_t = 0)]
    pub port: u16,
    /// Requested operation.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Version 0.1 command surface.
#[derive(Debug, Subcommand)]
#[allow(missing_docs)]
pub enum Command {
    /// Generate a shell completion script.
    Completions {
        /// Shell whose completion script should be generated.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Create a workspace with exactly one root node.
    Init {
        /// Human-readable workspace and root title.
        name: String,
    },
    /// Report workspace counts and format versions.
    Status,
    /// Create an online-safe backup.
    Backup {
        /// New backup destination.
        destination: PathBuf,
    },
    /// Restore a validated backup.
    Restore {
        /// Validated backup source.
        source: PathBuf,
        /// Permit atomic replacement of an existing workspace.
        #[arg(long)]
        overwrite: bool,
    },
    /// Export a complete whole-workspace interoperable snapshot.
    Export {
        /// New snapshot destination.
        destination: PathBuf,
        /// Snapshot representation.
        #[arg(long, value_enum)]
        format: SnapshotFormat,
    },
    /// Export one node or a bounded subtree as Markdown files.
    ExportNode {
        /// Node selected by ID, slug, or canonical path.
        selector: String,
        /// Existing output directory or new root Markdown file.
        destination: PathBuf,
        /// Include descendants of the selected node.
        #[arg(long)]
        subtree: bool,
        /// Maximum relative descendant depth; omitted means the complete subtree.
        #[arg(long, requires = "subtree")]
        depth: Option<u32>,
    },
    /// Import a snapshot into a new workspace.
    Import {
        /// Snapshot source.
        source: PathBuf,
        /// Snapshot representation.
        #[arg(long, value_enum)]
        format: SnapshotFormat,
    },
    /// Rebuild derived sections, references, and search rows.
    RebuildIndexes,
    /// Permanently discard historical revisions while retaining every current head.
    PruneHistory {
        /// Report the number of revisions that would be removed without writing.
        #[arg(long, conflicts_with_all = ["yes", "vacuum"])]
        dry_run: bool,
        /// Confirm permanent deletion of historical revisions.
        #[arg(long, required_unless_present = "dry_run")]
        yes: bool,
        /// Rebuild the database after pruning to reclaim filesystem space.
        #[arg(long, requires = "yes")]
        vacuum: bool,
    },
    /// Validate `SQLite` and `MDTree` invariants.
    Check,
    /// Validate `MDTree` invariants read-only with bounded resumable findings.
    Validate {
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Diagnose runtime and workspace health without repair.
    Doctor,
    /// Show the canonical root.
    Root,
    /// Show the complete tree from the root.
    Tree,
    /// Interactively browse the complete tree or a selected subtree.
    Browse {
        /// Optional subtree root selected by ID, slug, or path.
        selector: Option<String>,
    },
    /// Launch the local web tree viewer and editor.
    BrowseUi {
        /// Optional subtree root selected by ID, slug, or path.
        #[arg(conflicts_with = "also_workspaces")]
        selector: Option<String>,
        /// Keep the web UI server attached to this process instead of
        /// starting it in the background.
        #[arg(long)]
        foreground: bool,
        /// Additional workspace database paths to open alongside the primary
        /// workspace, switchable from a panel in the web UI. Each entry may
        /// carry an explicit display name as `<path>=<name>`; without one,
        /// the name is derived from the workspace file.
        #[arg(long = "also-workspace")]
        also_workspaces: Vec<String>,
    },
    /// Internal foreground process that owns one web UI server.
    #[command(name = "__serve-ui", hide = true)]
    ServeUi {
        /// Optional subtree root selected by ID, slug, or path.
        #[arg(conflicts_with = "also_workspaces")]
        selector: Option<String>,
        /// Open the default browser after the listener is ready.
        #[arg(long)]
        open_browser: bool,
        /// Additional workspace database paths to open alongside the primary
        /// workspace, switchable from a panel in the web UI. Each entry may
        /// carry an explicit display name as `<path>=<name>`; without one,
        /// the name is derived from the workspace file.
        #[arg(long = "also-workspace")]
        also_workspaces: Vec<String>,
    },
    /// Show one node selected by ID, slug, or path.
    Show { selector: String },
    /// Resolve up to 100 selectors in one ordered bounded request.
    BatchNode {
        #[arg(required = true, num_args = 1..=100)]
        selectors: Vec<String>,
    },
    /// Retrieve grouped child pages for up to 20 parents.
    BatchChildren {
        #[arg(required = true, num_args = 1..=20)]
        parents: Vec<String>,
        /// Per-parent page size; aggregate requested items may not exceed 100.
        #[arg(long, default_value_t = DEFAULT_PAGE_LIMIT)]
        limit: u32,
        /// Per-parent continuation as `PARENT=CURSOR`; repeat as needed.
        #[arg(long = "cursor")]
        cursors: Vec<String>,
    },
    /// Show database-side child, size, leaf, depth, and width statistics.
    Statistics { selector: String },
    /// Test reflexive ancestor containment directly.
    Contains {
        ancestor: String,
        descendant: String,
    },
    /// Show the reflexive lowest common ancestor of two nodes.
    LowestCommonAncestor { left: String, right: String },
    /// Show the endpoint-inclusive canonical path between two nodes.
    PathBetween { from: String, to: String },
    /// Show the canonical edge distance between two nodes.
    Distance { from: String, to: String },
    /// Enumerate leaf or internal subtree nodes in stable DFS order.
    Filter {
        selector: String,
        #[arg(long, value_enum)]
        predicate: FilterPredicate,
        /// Optional maximum relative depth including the selected root at zero.
        #[arg(long)]
        max_depth: Option<u32>,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Show the zero-based canonical child at one index.
    ChildAt { parent: String, index: u32 },
    /// Show direct previous and next canonical siblings.
    AdjacentSiblings { selector: String },
    /// Show direct children.
    Children {
        selector: String,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Show the direct parent.
    Parent { selector: String },
    /// Show root-to-parent ancestors.
    Ancestors { selector: String },
    /// Show all descendants with relative depth.
    Descendants {
        selector: String,
        /// Stable database-side traversal order.
        #[arg(long, value_enum, default_value_t = TraversalOrder::Dfs)]
        order: TraversalOrder,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Show nodes sharing the same parent.
    Siblings {
        selector: String,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Show a node and all descendants with relative depth.
    Subtree {
        selector: String,
        /// Stable database-side traversal order.
        #[arg(long, value_enum, default_value_t = TraversalOrder::Dfs)]
        order: TraversalOrder,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Select one structural relation; omit --relation for legacy bounded inspection.
    Navigate {
        selector: String,
        #[arg(long, value_enum)]
        relation: Option<NavigationRelation>,
        /// Maximum depth for legacy bounded inspection.
        #[arg(long)]
        depth: Option<u32>,
        /// Maximum items for paginated relations (1 through 100).
        #[arg(long)]
        limit: Option<u32>,
        /// Opaque continuation token returned by the preceding relation page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Show the current canonical path and breadcrumb.
    Path { selector: String },
    /// Search section-oriented content.
    Search {
        /// Free-text query.
        query: String,
        /// Structural scope: `current_node`, `subtree`, `siblings`, `parent_subtree`, `workspace`,
        /// or `linked`.
        #[arg(
            long,
            default_value = "workspace",
            value_parser = [
                "current_node",
                "subtree",
                "siblings",
                "parent_subtree",
                "workspace",
                "linked"
            ]
        )]
        scope: String,
        /// Node selector anchoring non-workspace scopes.
        #[arg(long)]
        scope_node: Option<String>,
        /// Eligible node type; repeat to accept any listed type.
        #[arg(long = "node-type")]
        node_types: Vec<String>,
        /// Required tag; repeat to require every listed tag.
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Eligible outgoing status-reference type; repeat to accept any listed status.
        #[arg(long = "status")]
        statuses: Vec<String>,
        /// Inclusive minimum relative depth.
        #[arg(long)]
        min_depth: Option<u32>,
        /// Inclusive maximum relative depth.
        #[arg(long)]
        max_depth: Option<u32>,
        /// Inclusive minimum creation timestamp in Unix milliseconds.
        #[arg(long)]
        created_from: Option<u64>,
        /// Inclusive maximum creation timestamp in Unix milliseconds.
        #[arg(long)]
        created_to: Option<u64>,
        /// Inclusive minimum update timestamp in Unix milliseconds.
        #[arg(long)]
        updated_from: Option<u64>,
        /// Inclusive maximum update timestamp in Unix milliseconds.
        #[arg(long)]
        updated_to: Option<u64>,
        /// Match leaves or internal nodes.
        #[arg(long, value_enum)]
        structure: Option<FilterPredicate>,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Locate the best structural destination for proposed information.
    Locate {
        query: String,
        #[arg(long)]
        node_type: Option<String>,
    },
    /// Inspect a bounded subtree summary.
    Inspect {
        selector: String,
        #[arg(long, default_value_t = 2)]
        depth: u32,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Discover structurally similar sibling examples.
    Examples {
        selector: String,
        #[arg(long, default_value_t = 5)]
        limit: u32,
    },
    /// Assemble bounded read or write context.
    Context {
        selector: String,
        #[arg(long, value_enum, default_value = "read")]
        mode: ContextMode,
        #[arg(long, default_value_t = 16384)]
        byte_limit: usize,
    },
    /// Create a child node atomically.
    Create {
        /// Parent selector.
        parent: String,
        /// New node title.
        title: String,
        /// Markdown body; defaults to a title heading.
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Replace Markdown content with optimistic concurrency.
    Update {
        /// Node selector.
        selector: String,
        /// Complete replacement Markdown.
        #[arg(long)]
        content: String,
        /// Version observed by the caller.
        #[arg(long)]
        expected_version: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Rename a node and regenerate its collision-safe slug.
    Rename {
        /// Node selector.
        selector: String,
        /// Replacement title.
        title: String,
        /// Version observed by the caller.
        #[arg(long)]
        expected_version: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Move a complete subtree below another parent.
    Move {
        /// Node selector.
        selector: String,
        /// New parent selector.
        parent: String,
        /// Version observed by the caller.
        #[arg(long)]
        expected_version: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Clone a subtree atomically with new identities and remapped internal references.
    CloneSubtree {
        /// Existing subtree root selected by ID, slug, or canonical path.
        source: String,
        /// Existing destination parent selected by ID, slug, or canonical path.
        destination_parent: String,
        /// Version observed on the source before cloning.
        #[arg(long)]
        expected_version: u64,
        /// Optional zero-based position among the destination's children.
        #[arg(long)]
        sibling_order: Option<u32>,
        /// Validate and report the clone plan without writing.
        #[arg(long)]
        dry_run: bool,
        /// Identity recorded on every immutable clone revision.
        #[arg(long)]
        author: Option<String>,
        /// Reason recorded on every immutable clone revision.
        #[arg(long)]
        change_summary: Option<String>,
    },
    /// Apply unrelated moves and removals as one guarded transaction.
    #[command(
        long_about = "Apply unrelated moves and removals as one guarded transaction.\n\nThe JSON document accepts `moves` and `removals` arrays. Each move supplies `selector`, `destination_parent`, `expected_version`, and optional `sibling_order`; each removal supplies `selector` and `expected_version`. The complete request is validated before one all-or-nothing transaction."
    )]
    AtomicTreeBatch {
        /// JSON request file, or `-` to read standard input.
        file: String,
        /// Validate the complete batch without changing the workspace.
        #[arg(long)]
        dry_run: bool,
    },
    /// Apply a heterogeneous JSON mutation batch as one transaction.
    #[command(
        long_about = "Apply a heterogeneous JSON mutation batch as one transaction.\n\nThe JSON document contains an `operations` array with 1 through 50 ordered operations. Supported `kind` values are `create`, `update`, `rename`, `move`, `reorder`, `remove`, and `set_references`. Create operations may declare labels that later operations use as selectors. Every existing-node mutation carries its observed version; any invalid operation rolls back the complete batch."
    )]
    MutationBatch {
        /// JSON request file, or `-` to read standard input.
        file: String,
        /// Validate and resolve all operations without committing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a subtree, optionally reporting impact only.
    Remove {
        /// Node selector.
        selector: String,
        /// Version observed by the caller.
        #[arg(long)]
        expected_version: u64,
        /// Report impact without mutation.
        #[arg(long)]
        dry_run: bool,
    },
    /// List immutable node history newest-first.
    History {
        selector: String,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Show one exact retained immutable revision without restoring it.
    Revision { selector: String, version: u64 },
    /// Diff any two retained versions.
    Diff {
        selector: String,
        from: u64,
        to: u64,
    },
    /// Compare two current subtrees with bounded resumable output.
    SubtreeDiff {
        /// Root of the left/current subtree state.
        from_selector: String,
        /// Root of the right/current subtree state.
        to_selector: String,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Restore a retained version as a new head revision.
    RestoreVersion {
        selector: String,
        version: u64,
        #[arg(long)]
        expected_version: u64,
    },
    /// List outgoing typed references.
    References {
        selector: String,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// List incoming typed references.
    Backlinks {
        selector: String,
        #[command(flatten)]
        pagination: PaginationArgs,
    },
    /// Add one explicit typed reference.
    ReferenceAdd {
        source: String,
        target: String,
        relation: String,
        #[arg(long)]
        expected_version: u64,
    },
    /// Remove one explicit typed reference.
    ReferenceRemove {
        source: String,
        target: String,
        relation: String,
        #[arg(long)]
        expected_version: u64,
    },
    /// Report unresolved references.
    Unresolved,
    /// Report likely duplicate nodes.
    Duplicates,
}

#[derive(Debug, Deserialize)]
struct CliAtomicTreeBatch {
    #[serde(default)]
    moves: Vec<CliAtomicTreeMove>,
    #[serde(default)]
    removals: Vec<CliAtomicTreeRemoval>,
}

#[derive(Debug, Deserialize)]
struct CliAtomicTreeMove {
    selector: String,
    destination_parent: String,
    expected_version: u64,
    sibling_order: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct CliAtomicTreeRemoval {
    selector: String,
    expected_version: u64,
}

#[derive(Debug, Deserialize)]
struct CliMutationBatch {
    operations: Vec<CliBatchOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CliBatchOperation {
    Create {
        label: Option<String>,
        parent: String,
        title: String,
        content: Option<String>,
        requested_id: Option<String>,
        sibling_order: Option<u32>,
    },
    Update {
        selector: String,
        content: String,
        expected_version: u64,
    },
    Rename {
        selector: String,
        title: String,
        expected_version: u64,
    },
    Move {
        selector: String,
        destination_parent: String,
        expected_version: u64,
        sibling_order: Option<u32>,
    },
    Reorder {
        selector: String,
        sibling_order: u32,
        expected_version: u64,
    },
    Remove {
        selector: String,
        expected_version: u64,
    },
    SetReferences {
        selector: String,
        expected_version: u64,
        references: Vec<CliBatchReference>,
    },
}

#[derive(Debug, Deserialize)]
struct CliBatchReference {
    relation: String,
    target: String,
}

/// Context assembly purpose.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[allow(missing_docs)]
pub enum ContextMode {
    Read,
    Write,
}

/// Parses process arguments, executes one command, and returns a stable exit code.
#[must_use]
pub fn main_entry() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init();
    match execute(&Cli::parse(), &mut io::stdout()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let _ = writeln!(io::stderr(), "{error}");
            ExitCode::from(EXIT_OPERATIONAL)
        }
    }
}

/// Executes a parsed command against an injected output writer.
///
/// # Errors
///
/// Returns an operational error when workspace I/O, parsing, or a service call fails.
#[allow(clippy::too_many_lines)]
pub fn execute(cli: &Cli, output: &mut dyn Write) -> anyhow::Result<u8> {
    let Some(command) = cli.command.as_ref() else {
        return execute_onboarding(cli, output);
    };
    if let Command::Completions { shell } = command {
        let mut command = Cli::command();
        clap_complete::generate(*shell, &mut command, "mdtree", output);
        return Ok(EXIT_OK);
    }
    if let Command::Init { name } = command {
        let workspace = required_workspace(cli);
        let root = new_root(name)?;
        mdtree_sqlite::create_workspace(workspace, name, &root)?;
        return emit_ok(output, cli.output, "initialized");
    }
    if let Command::Import { source, format } = command {
        let workspace = required_workspace(cli);
        match format {
            SnapshotFormat::Json => {
                let bytes = std::fs::read(source)?;
                let plan = plan_json_import(&bytes)?;
                if !plan.validation.is_valid() {
                    emit(output, cli.output, &plan.validation)?;
                    return Ok(EXIT_INVALID);
                }
                import_snapshot_new(workspace, &plan.snapshot)?;
            }
            SnapshotFormat::Markdown => import_markdown_snapshot_new(source, workspace)?,
        }
        return emit_ok(output, cli.output, "imported");
    }
    if let Command::Restore { source, overwrite } = command {
        restore_workspace(source, required_workspace(cli), *overwrite)?;
        return emit_ok(output, cli.output, "restored");
    }
    if let Command::BrowseUi {
        selector,
        foreground,
        also_workspaces,
    } = command
    {
        if *foreground {
            return serve_web_ui(cli, selector.as_deref(), also_workspaces, !cli.no_open);
        }
        return launch_web_ui(cli, selector.as_deref(), also_workspaces, output);
    }
    if let Command::ServeUi {
        selector,
        open_browser,
        also_workspaces,
    } = command
    {
        return serve_web_ui(cli, selector.as_deref(), also_workspaces, *open_browser);
    }
    if let Command::Doctor = command {
        let report = doctor_workspace(required_workspace(cli));
        emit(output, cli.output, &report)?;
        return Ok(if report.findings.iter().any(|item| item.blocking) {
            EXIT_INVALID
        } else {
            EXIT_OK
        });
    }
    let workspace = required_workspace(cli);
    let mut store = open_store(workspace)?;
    match command {
        Command::Status => emit(
            output,
            cli.output,
            &workspace_status(store.connection(), workspace)?,
        )?,
        Command::Backup { destination } => {
            backup_workspace(&store, destination)?;
            emit(
                output,
                cli.output,
                &serde_json::json!({"status":"backed_up"}),
            )?;
        }
        Command::Export {
            destination,
            format,
        } => export_complete_workspace_snapshot(&store, destination, *format)?,
        Command::ExportNode {
            selector,
            destination,
            subtree,
            depth,
        } => {
            let id = resolve_id(&store, selector)?;
            let max_depth = if *subtree { *depth } else { Some(0) };
            let exported_files = export_markdown_node(&store, id, destination, max_depth)?;
            emit(
                output,
                cli.output,
                &serde_json::json!({
                    "exported_files": exported_files,
                }),
            )?;
        }
        Command::RebuildIndexes => {
            store.rebuild_derived(&SystemUlidGenerator)?;
            emit(output, cli.output, &serde_json::json!({"status":"rebuilt"}))?;
        }
        Command::PruneHistory {
            dry_run,
            yes: _,
            vacuum,
        } => {
            let report = if *dry_run {
                store.plan_history_prune()?
            } else {
                store.prune_history()?
            };
            if *vacuum {
                store.vacuum()?;
            }
            emit(
                output,
                cli.output,
                &serde_json::json!({
                    "status": if *dry_run { "planned" } else { "applied" },
                    "nodes": report.node_count,
                    "revisions_before": report.revisions_before,
                    "revisions_removed": report.revisions_removed,
                    "revisions_retained": report.revisions_retained,
                    "vacuumed": *vacuum,
                }),
            )?;
        }
        Command::Check => {
            let report = check_workspace(&store)?;
            let code = if report.status == CheckStatus::Healthy {
                EXIT_OK
            } else {
                EXIT_INVALID
            };
            emit(output, cli.output, &report)?;
            return Ok(code);
        }
        Command::Validate { pagination } => {
            let (limit, cursor) = pagination.validated()?;
            let report = store.integrity_page(limit, cursor.as_ref())?;
            let code = if report.healthy {
                EXIT_OK
            } else {
                EXIT_INVALID
            };
            emit(output, cli.output, &report)?;
            return Ok(code);
        }
        Command::Root => {
            emit(output, cli.output, &store.root_projection()?)?;
        }
        Command::Tree => emit_depths(
            output,
            cli.output,
            &store,
            store.subtree(store.root()?.id())?,
        )?,
        Command::Browse { selector } => {
            let root = selector
                .as_deref()
                .map(|value| resolve_id(&store, value))
                .transpose()?
                .unwrap_or(store.root()?.id());
            browse_workspace(cli.output, &store, root, output)?;
        }
        Command::Show { selector } => {
            emit(
                output,
                cli.output,
                &vec![resolve_projection(&store, selector)?],
            )?;
        }
        Command::BatchNode { selectors } => {
            emit(output, cli.output, &store.batch_node_lookup(selectors)?)?;
        }
        Command::BatchChildren {
            parents,
            limit,
            cursors,
        } => {
            let cursor_pairs = cursors
                .iter()
                .map(|value| {
                    value
                        .split_once('=')
                        .map(|(parent, cursor)| (parent.to_owned(), cursor.to_owned()))
                        .ok_or_else(|| anyhow::anyhow!("cursor must use PARENT=CURSOR"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let requests = parents
                .iter()
                .map(|parent| BatchChildrenRequest {
                    parent: parent.clone(),
                    limit: *limit,
                    cursor: cursor_pairs
                        .iter()
                        .find(|(candidate, _)| candidate == parent)
                        .map(|(_, cursor)| cursor.clone()),
                })
                .collect::<Vec<_>>();
            emit(output, cli.output, &store.batch_children_lookup(&requests)?)?;
        }
        Command::Statistics { selector } => {
            emit(
                output,
                cli.output,
                &store.tree_statistics(resolve_id(&store, selector)?)?,
            )?;
        }
        Command::Contains {
            ancestor,
            descendant,
        } => {
            let ancestor = resolve_id(&store, ancestor)?;
            let descendant = resolve_id(&store, descendant)?;
            emit(
                output,
                cli.output,
                &store.contains_ancestor(ancestor, descendant)?,
            )?;
        }
        Command::LowestCommonAncestor { left, right } => {
            let left = resolve_id(&store, left)?;
            let right = resolve_id(&store, right)?;
            emit(
                output,
                cli.output,
                &store.lowest_common_ancestor(left, right)?,
            )?;
        }
        Command::PathBetween { from, to } => {
            let from = resolve_id(&store, from)?;
            let to = resolve_id(&store, to)?;
            emit(output, cli.output, &store.path_between(from, to)?)?;
        }
        Command::Distance { from, to } => {
            let from = resolve_id(&store, from)?;
            let to = resolve_id(&store, to)?;
            emit(output, cli.output, &store.tree_distance(from, to)?)?;
        }
        Command::Filter {
            selector,
            predicate,
            max_depth,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            emit(
                output,
                cli.output,
                &store.filtered_subtree_page(
                    resolve_id(&store, selector)?,
                    (*predicate).into(),
                    *max_depth,
                    limit,
                    cursor.as_ref(),
                )?,
            )?;
        }
        Command::ChildAt { parent, index } => {
            let parent = resolve_id(&store, parent)?;
            emit(output, cli.output, &store.child_at(parent, *index)?)?;
        }
        Command::AdjacentSiblings { selector } => {
            let id = resolve_id(&store, selector)?;
            emit(output, cli.output, &store.adjacent_siblings(id)?)?;
        }
        Command::Children {
            selector,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            let page =
                store.children_page(resolve_id(&store, selector)?, limit, cursor.as_ref())?;
            emit(output, cli.output, &page)?;
        }
        Command::Parent { selector } => {
            let id = resolve_id(&store, selector)?;
            let parent = store.parent_projection(id)?;
            emit(output, cli.output, &parent.into_iter().collect::<Vec<_>>())?;
        }
        Command::Ancestors { selector } => emit_depths(
            output,
            cli.output,
            &store,
            store.ancestors(resolve_id(&store, selector)?)?,
        )?,
        Command::Descendants {
            selector,
            order,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            let page = store.descendants_page_ordered(
                resolve_id(&store, selector)?,
                (*order).into(),
                limit,
                cursor.as_ref(),
            )?;
            emit(output, cli.output, &page)?;
        }
        Command::Siblings {
            selector,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            let page =
                store.siblings_page(resolve_id(&store, selector)?, limit, cursor.as_ref())?;
            emit(output, cli.output, &page)?;
        }
        Command::Subtree {
            selector,
            order,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            let page = store.subtree_page_ordered(
                resolve_id(&store, selector)?,
                (*order).into(),
                limit,
                cursor.as_ref(),
            )?;
            emit(output, cli.output, &page)?;
        }
        Command::Navigate {
            selector,
            relation,
            depth,
            limit,
            cursor,
        } => {
            let id = resolve_id(&store, selector)?;
            if let Some(relation) = relation {
                if depth.is_some() {
                    anyhow::bail!("depth is supported only when relation is omitted");
                }
                match relation {
                    NavigationRelation::Parent | NavigationRelation::Ancestors
                        if limit.is_some() || cursor.is_some() =>
                    {
                        anyhow::bail!("limit and cursor are not supported for parent or ancestors");
                    }
                    NavigationRelation::Parent => {
                        let parent = store.parent_projection(id)?;
                        emit(output, cli.output, &parent.into_iter().collect::<Vec<_>>())?;
                    }
                    NavigationRelation::Ancestors => {
                        emit_depths(output, cli.output, &store, store.ancestors(id)?)?;
                    }
                    NavigationRelation::Children => {
                        let (limit, cursor) = validated_navigation_page(*limit, cursor.as_deref())?;
                        emit(
                            output,
                            cli.output,
                            &store.children_page(id, limit, cursor.as_ref())?,
                        )?;
                    }
                    NavigationRelation::Descendants => {
                        let (limit, cursor) = validated_navigation_page(*limit, cursor.as_deref())?;
                        emit(
                            output,
                            cli.output,
                            &store.descendants_page(id, limit, cursor.as_ref())?,
                        )?;
                    }
                    NavigationRelation::Siblings => {
                        let (limit, cursor) = validated_navigation_page(*limit, cursor.as_deref())?;
                        emit(
                            output,
                            cli.output,
                            &store.siblings_page(id, limit, cursor.as_ref())?,
                        )?;
                    }
                    NavigationRelation::Subtree => {
                        let (limit, cursor) = validated_navigation_page(*limit, cursor.as_deref())?;
                        emit(
                            output,
                            cli.output,
                            &store.subtree_page(id, limit, cursor.as_ref())?,
                        )?;
                    }
                }
            } else {
                let (limit, cursor) = validated_navigation_page(*limit, cursor.as_deref())?;
                emit(
                    output,
                    cli.output,
                    &store.inspect_subtree_page(
                        "navigate",
                        id,
                        depth.unwrap_or(2),
                        limit,
                        cursor.as_ref(),
                    )?,
                )?;
            }
        }
        Command::Path { selector } => {
            let id = resolve_id(&store, selector)?;
            emit(
                output,
                cli.output,
                &serde_json::json!({"node_id":id,"path":store.canonical_path(id)?,"breadcrumb":store.breadcrumb(id)?}),
            )?;
        }
        Command::Search {
            query,
            scope,
            scope_node,
            node_types,
            tags,
            statuses,
            min_depth,
            max_depth,
            created_from,
            created_to,
            updated_from,
            updated_to,
            structure,
            pagination,
        } => {
            let scope = parse_search_scope(scope)?;
            let scope_node = scope_node
                .as_deref()
                .map(|selector| resolve_id(&store, selector))
                .transpose()?;
            if scope != SearchScope::Workspace && scope_node.is_none() {
                anyhow::bail!("scope_node is required outside workspace scope");
            }
            let (limit, cursor) = pagination.validated()?;
            let node_types = node_types
                .iter()
                .map(|value| NodeType::from_str(value))
                .collect::<Result<Vec<_>, _>>()?;
            let statuses = statuses
                .iter()
                .map(|value| ReferenceType::from_str(value))
                .collect::<Result<Vec<_>, _>>()?;
            let filters = SearchFilters {
                node_types,
                tags: tags.clone(),
                statuses,
                min_depth: *min_depth,
                max_depth: *max_depth,
                created_from: *created_from,
                created_to: *created_to,
                updated_from: *updated_from,
                updated_to: *updated_to,
                structure: structure.map(Into::into),
            };
            filters.validate(scope).map_err(anyhow::Error::msg)?;
            emit(
                output,
                cli.output,
                &store.search_content_page(
                    &SearchRequest {
                        query: query.clone(),
                        scope,
                        scope_node,
                        filters,
                        limit: limit.get(),
                        offset: 0,
                        prefix_last_token: true,
                    },
                    limit,
                    cursor.as_ref(),
                )?,
            )?;
        }
        Command::Locate { query, node_type } => {
            let kind = node_type.as_deref().map(NodeType::from_str).transpose()?;
            emit(
                output,
                cli.output,
                &store.locate_target(query, kind.as_ref())?,
            )?;
        }
        Command::Inspect {
            selector,
            depth,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            emit(
                output,
                cli.output,
                &store.inspect_subtree_page(
                    "inspect",
                    resolve_id(&store, selector)?,
                    *depth,
                    limit,
                    cursor.as_ref(),
                )?,
            )?;
        }
        Command::Examples { selector, limit } => {
            let id = resolve_id(&store, selector)?;
            let node = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("node not found"))?;
            emit(
                output,
                cli.output,
                &store.examples_for(id, node.fields().metadata.node_type.as_ref(), *limit)?,
            )?;
        }
        Command::Context {
            selector,
            mode,
            byte_limit,
        } => {
            let id = resolve_id(&store, selector)?;
            match mode {
                ContextMode::Read => {
                    emit(output, cli.output, &store.read_context(id, *byte_limit)?)?;
                }
                ContextMode::Write => {
                    emit(output, cli.output, &store.write_context(id, *byte_limit)?)?;
                }
            }
        }
        Command::Create {
            parent,
            title,
            content,
            dry_run,
        } => {
            let parent_id = resolve_id(&store, parent)?;
            let (slug, sibling_order) = store.next_child_placement(parent_id, title)?;
            let id = NodeId::new(SystemUlidGenerator.generate());
            let markdown = content.clone().unwrap_or_else(|| format!("# {title}\n"));
            let now = now_millis()?;
            let prepared = prepare_cli_mutation(
                id,
                Some(parent_id),
                slug,
                NodeMetadata::new(title),
                markdown,
                sibling_order,
                1,
                now,
                now,
                "Create node",
            )?;
            if *dry_run {
                emit(
                    output,
                    cli.output,
                    &serde_json::json!({"status":"planned","node_id":id,"parent_id":parent_id,"slug":prepared.node.fields().slug}),
                )?;
                return Ok(EXIT_OK);
            }
            store.create_node(&prepared.node, &prepared.revision, &prepared.derived)?;
            emit(
                output,
                cli.output,
                &serde_json::json!({"status":"created","node_id":id,"version":1}),
            )?;
        }
        Command::Update {
            selector,
            content,
            expected_version,
            dry_run,
        } => {
            let current = current_node(&store, selector)?;
            let f = current.fields();
            let prepared = prepare_cli_mutation(
                current.id(),
                current.parent_id(),
                f.slug.clone(),
                f.metadata.clone(),
                content.clone(),
                f.sibling_order,
                expected_version + 1,
                f.created_at,
                now_millis()?,
                "Update content",
            )?;
            if *dry_run {
                emit(
                    output,
                    cli.output,
                    &serde_json::json!({"status":"planned","node_id":prepared.node.id(),"version":prepared.node.fields().version}),
                )?;
                return Ok(EXIT_OK);
            }
            let outcome =
                apply_change(&mut store, &prepared, *expected_version, ChangeKind::Update)?;
            emit_mutation(output, cli.output, outcome)?;
        }
        Command::Rename {
            selector,
            title,
            expected_version,
            dry_run,
        } => {
            let current = current_node(&store, selector)?;
            let f = current.fields();
            let mut metadata = f.metadata.clone();
            metadata.title.clone_from(title);
            let slug = if let Some(parent) = current.parent_id() {
                store.next_child_placement(parent, title)?.0
            } else {
                mdtree_core::generate_slug(title, std::iter::empty::<&Slug>())
            };
            let prepared = prepare_cli_mutation(
                current.id(),
                current.parent_id(),
                slug,
                metadata,
                f.markdown_content.clone(),
                f.sibling_order,
                expected_version + 1,
                f.created_at,
                now_millis()?,
                "Rename node",
            )?;
            if *dry_run {
                emit(
                    output,
                    cli.output,
                    &serde_json::json!({"status":"planned","node_id":prepared.node.id(),"slug":prepared.node.fields().slug}),
                )?;
                return Ok(EXIT_OK);
            }
            let outcome =
                apply_change(&mut store, &prepared, *expected_version, ChangeKind::Rename)?;
            emit_mutation(output, cli.output, outcome)?;
        }
        Command::Move {
            selector,
            parent,
            expected_version,
            dry_run,
        } => {
            let current = current_node(&store, selector)?;
            let parent_id = resolve_id(&store, parent)?;
            let f = current.fields();
            let (slug, order) = store.next_child_placement(parent_id, &f.metadata.title)?;
            let prepared = prepare_cli_mutation(
                current.id(),
                Some(parent_id),
                slug,
                f.metadata.clone(),
                f.markdown_content.clone(),
                order,
                expected_version + 1,
                f.created_at,
                now_millis()?,
                "Move subtree",
            )?;
            if *dry_run {
                emit(
                    output,
                    cli.output,
                    &serde_json::json!({"status":"planned","node_id":prepared.node.id(),"parent_id":parent_id}),
                )?;
                return Ok(EXIT_OK);
            }
            let outcome = apply_change(&mut store, &prepared, *expected_version, ChangeKind::Move)?;
            emit_mutation(output, cli.output, outcome)?;
        }
        Command::CloneSubtree {
            source,
            destination_parent,
            expected_version,
            sibling_order,
            dry_run,
            author,
            change_summary,
        } => {
            let request = CloneSubtreeRequest {
                source_id: resolve_id(&store, source)?,
                destination_parent_id: resolve_id(&store, destination_parent)?,
                expected_version: *expected_version,
                sibling_order: *sibling_order,
                dry_run: *dry_run,
                created_at: now_millis()?,
                created_by: author.clone(),
                change_summary: change_summary.clone(),
            };
            emit(
                output,
                cli.output,
                &store.clone_subtree(&request, &SystemUlidGenerator)?,
            )?;
        }
        Command::AtomicTreeBatch { file, dry_run } => {
            let bytes = if file == "-" {
                let mut bytes = String::new();
                io::stdin().read_to_string(&mut bytes)?;
                bytes
            } else {
                std::fs::read_to_string(file)?
            };
            let request: CliAtomicTreeBatch = serde_json::from_str(&bytes)?;
            let now = now_millis()?;
            let mut moves = Vec::with_capacity(request.moves.len());
            for item in request.moves {
                let current = current_node(&store, &item.selector)?;
                let parent_id = resolve_id(&store, &item.destination_parent)?;
                let fields = current.fields();
                let (slug, default_order) =
                    store.next_child_placement(parent_id, &fields.metadata.title)?;
                let prepared = prepare_cli_mutation(
                    current.id(),
                    Some(parent_id),
                    slug,
                    fields.metadata.clone(),
                    fields.markdown_content.clone(),
                    item.sibling_order.unwrap_or(default_order),
                    item.expected_version + 1,
                    fields.created_at,
                    now,
                    "Atomic tree batch move",
                )?;
                moves.push(AtomicTreeMove {
                    prepared,
                    expected_version: item.expected_version,
                });
            }
            let removals = request
                .removals
                .into_iter()
                .map(|item| {
                    Ok(AtomicTreeRemoval {
                        node_id: resolve_id(&store, &item.selector)?,
                        expected_version: item.expected_version,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            emit(
                output,
                cli.output,
                &store.apply_atomic_tree_batch(&moves, &removals, *dry_run)?,
            )?;
        }
        Command::MutationBatch { file, dry_run } => {
            let bytes = if file == "-" {
                let mut bytes = String::new();
                io::stdin().read_to_string(&mut bytes)?;
                bytes
            } else {
                std::fs::read_to_string(file)?
            };
            let request: CliMutationBatch = serde_json::from_str(&bytes)?;
            let operations = prepare_cli_batch(&store, request, now_millis()?)?;
            emit(
                output,
                cli.output,
                &store.apply_mutation_batch(&operations, *dry_run)?,
            )?;
        }
        Command::Remove {
            selector,
            expected_version,
            dry_run,
        } => {
            let id = resolve_id(&store, selector)?;
            emit(
                output,
                cli.output,
                &store.remove_subtree(id, *expected_version, *dry_run)?,
            )?;
        }
        Command::History {
            selector,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            emit(
                output,
                cli.output,
                &store.revision_history_page(
                    resolve_id(&store, selector)?,
                    limit,
                    cursor.as_ref(),
                )?,
            )?;
        }
        Command::Revision { selector, version } => {
            let id = resolve_id(&store, selector)?;
            emit(
                output,
                cli.output,
                &store
                    .revision(id, *version)?
                    .ok_or_else(|| anyhow::anyhow!("version {version} not found"))?,
            )?;
        }
        Command::Diff { selector, from, to } => {
            let id = resolve_id(&store, selector)?;
            let left = store
                .revision(id, *from)?
                .ok_or_else(|| anyhow::anyhow!("version {from} not found"))?;
            let right = store
                .revision(id, *to)?
                .ok_or_else(|| anyhow::anyhow!("version {to} not found"))?;
            emit(
                output,
                cli.output,
                &mdtree_core::diff_revisions(&left, &right),
            )?;
        }
        Command::SubtreeDiff {
            from_selector,
            to_selector,
            pagination,
        } => {
            let from = resolve_id(&store, from_selector)?;
            let to = resolve_id(&store, to_selector)?;
            let (limit, cursor) = pagination.validated()?;
            emit(
                output,
                cli.output,
                &store.subtree_diff_page(from, to, limit, cursor.as_ref())?,
            )?;
        }
        Command::RestoreVersion {
            selector,
            version,
            expected_version,
        } => {
            let id = resolve_id(&store, selector)?;
            let outcome = store.restore_version(
                id,
                *version,
                *expected_version,
                now_millis()?,
                Some("cli".into()),
                &SystemUlidGenerator,
            )?;
            emit_mutation(output, cli.output, outcome)?;
        }
        Command::References {
            selector,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            emit(
                output,
                cli.output,
                &store.outgoing_references_page(
                    resolve_id(&store, selector)?,
                    limit,
                    cursor.as_ref(),
                )?,
            )?;
        }
        Command::Backlinks {
            selector,
            pagination,
        } => {
            let (limit, cursor) = pagination.validated()?;
            emit(
                output,
                cli.output,
                &store.backlinks_page(resolve_id(&store, selector)?, limit, cursor.as_ref())?,
            )?;
        }
        Command::ReferenceAdd {
            source,
            target,
            relation,
            expected_version,
        } => {
            mutate_reference(
                &mut store,
                source,
                target,
                relation,
                *expected_version,
                true,
            )?;
            emit_ok(output, cli.output, "reference_added")?;
        }
        Command::ReferenceRemove {
            source,
            target,
            relation,
            expected_version,
        } => {
            mutate_reference(
                &mut store,
                source,
                target,
                relation,
                *expected_version,
                false,
            )?;
            emit_ok(output, cli.output, "reference_removed")?;
        }
        Command::Unresolved => emit(output, cli.output, &store.unresolved_references()?)?,
        Command::Duplicates => emit(output, cli.output, &store.duplicate_candidates()?)?,
        Command::Completions { .. }
        | Command::Init { .. }
        | Command::Import { .. }
        | Command::Restore { .. }
        | Command::BrowseUi { .. }
        | Command::ServeUi { .. }
        | Command::Doctor => unreachable!(),
    }
    Ok(EXIT_OK)
}

fn execute_onboarding(cli: &Cli, output: &mut dyn Write) -> anyhow::Result<u8> {
    let workspace = required_workspace(cli);
    if !workspace.exists() && cli.workspace.is_none() {
        return emit_onboarding(
            output,
            cli.output,
            "workspace_missing",
            &format!(
                "No MDTree workspace found at {}.\n\nCreate the database and its root node:\n  mdtree init \"My Knowledge Base\"\n\nThen run `mdtree` again to open its web UI.",
                workspace.display()
            ),
        );
    }
    if workspace.exists() && workspace.metadata()?.len() == 0 {
        return emit_onboarding(
            output,
            cli.output,
            "workspace_empty",
            &format!(
                "{} is an empty file, not an initialized MDTree workspace.\n\nRemove or rename it, then create the database and root node:\n  mdtree init \"My Knowledge Base\"",
                workspace.display()
            ),
        );
    }

    // Preserve the normal workspace diagnostics in the launching process so a
    // failed startup cannot be mistaken for a successfully detached server.
    drop(open_store(workspace)?);
    launch_web_ui(cli, None, &[], output)
}

/// Splits an `--also-workspace` value into its path and optional display
/// name, on the *last* `=` so a Windows drive-letter path (`C:\...`) never
/// gets mistaken for a name separator.
fn parse_workspace_arg(raw: &str) -> mdtree_web::WorkspaceSource {
    raw.rfind('=').map_or_else(
        || mdtree_web::WorkspaceSource {
            path: PathBuf::from(raw),
            name: None,
        },
        |split| mdtree_web::WorkspaceSource {
            path: PathBuf::from(&raw[..split]),
            name: Some(raw[split + 1..].to_string()),
        },
    )
}

fn serve_web_ui(
    cli: &Cli,
    selector: Option<&str>,
    also_workspaces: &[String],
    open_browser: bool,
) -> anyhow::Result<u8> {
    let mut workspaces = vec![mdtree_web::WorkspaceSource {
        path: required_workspace(cli).to_path_buf(),
        name: cli.workspace_name.clone(),
    }];
    workspaces.extend(also_workspaces.iter().map(|raw| parse_workspace_arg(raw)));
    let options = mdtree_web::BrowseUiOptions {
        selector: selector.map(str::to_owned),
        open_browser,
        port: cli.port,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(mdtree_web::run(&workspaces, options))?;
    Ok(EXIT_OK)
}

fn launch_web_ui(
    cli: &Cli,
    selector: Option<&str>,
    also_workspaces: &[String],
    output: &mut dyn Write,
) -> anyhow::Result<u8> {
    let mut command = ProcessCommand::new(std::env::current_exe()?);
    command
        .arg("--workspace")
        .arg(required_workspace(cli))
        .arg("__serve-ui")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(name) = &cli.workspace_name {
        command.arg("--workspace-name").arg(name);
    }
    command.arg("--port").arg(cli.port.to_string());
    if let Some(selector) = selector {
        command.arg(selector);
    }
    for extra in also_workspaces {
        command.arg("--also-workspace").arg(extra);
    }
    if !cli.no_open {
        command.arg("--open-browser");
    }

    let mut child = command.spawn()?;
    let mut stdout = io::BufReader::new(child.stdout.take().expect("piped child stdout"));
    let mut url = String::new();
    stdout.read_line(&mut url)?;
    let url = url.trim();
    if !url.starts_with("http://") || url.chars().any(char::is_whitespace) {
        let mut details = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut details);
        }
        let status = child.wait()?;
        let details = details.trim();
        if details.is_empty() {
            anyhow::bail!("web UI server failed to start ({status})");
        }
        anyhow::bail!("web UI server failed to start: {details}");
    }

    writeln!(output, "{url}")?;
    output.flush()?;
    Ok(EXIT_OK)
}

fn browse_workspace(
    format: OutputFormat,
    store: &SqliteStore,
    root: NodeId,
    output: &mut dyn Write,
) -> anyhow::Result<()> {
    if format != OutputFormat::Text {
        anyhow::bail!("browse is interactive and does not support --output json or jsonl");
    }
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        browse_interactive(store, root, output)
    } else {
        browse_line_mode(store, root, &mut io::stdin().lock(), output)
    }
}

fn emit_onboarding(
    output: &mut dyn Write,
    format: OutputFormat,
    status: &str,
    message: &str,
) -> anyhow::Result<u8> {
    if format == OutputFormat::Text {
        writeln!(output, "{message}")?;
    } else {
        emit(
            output,
            format,
            &serde_json::json!({"status": status, "message": message}),
        )?;
    }
    Ok(EXIT_OK)
}

fn open_store(path: &std::path::Path) -> anyhow::Result<SqliteStore> {
    if !path.exists() {
        return Err(workspace_open_diagnostic(
            path,
            "the workspace file does not exist or its parent directory is inaccessible",
            "file does not exist",
        ));
    }
    SqliteStore::open(path).map_err(|error| workspace_open_error(path, &error))
}

/// The only CLI boundary allowed to invoke complete snapshot serialization.
///
/// Targeted commands, including `show`, `children`, traversal, and
/// `export-node`, must continue to use their bounded storage projections.
fn export_complete_workspace_snapshot(
    store: &SqliteStore,
    destination: &Path,
    format: SnapshotFormat,
) -> anyhow::Result<()> {
    match format {
        SnapshotFormat::Json => std::fs::write(destination, export_snapshot_json(store)?)?,
        SnapshotFormat::Markdown => export_markdown_snapshot(store, destination)?,
    }
    Ok(())
}

fn workspace_open_error(path: &std::path::Path, error: &WorkspaceError) -> anyhow::Error {
    let raw_reason = error.to_string();
    let reason = if raw_reason.contains("not a database") {
        "the file is not a valid SQLite database; it may be corrupt or may be the wrong file"
    } else if raw_reason.contains("no such table") {
        "the SQLite file is not an initialized MDTree workspace"
    } else if raw_reason.contains("permission")
        || raw_reason.contains("readonly")
        || raw_reason.contains("read-only")
        || raw_reason.contains("unable to open")
    {
        "SQLite could not access the file; check the file and directory permissions"
    } else {
        "the workspace could not be validated; it may be corrupt or incompatible"
    };
    workspace_open_diagnostic(path, reason, &raw_reason)
}

fn workspace_open_diagnostic(
    path: &std::path::Path,
    reason: &str,
    raw_reason: &str,
) -> anyhow::Error {
    let command = retry_command();
    let recovery = if raw_reason == "file does not exist" {
        "  - To create the default workspace and its root node, run:\n    mdtree init \"My Knowledge Base\""
    } else if reason.contains("permission") || reason.contains("access") {
        "  - Fix the file and parent-directory permissions, or choose a workspace you can read and write."
    } else {
        "  - If this is a disposable invalid `.mdtree` file, move it aside (recommended) or delete it, then run `mdtree init \"My Knowledge Base\"`."
    };
    anyhow::anyhow!(
        "Could not open MDTree workspace {}.\nReason: {reason}.\nUnderlying error: {raw_reason}\n\nWhat to do:\n  - Check that the path is correct and that you can read and write the file and its directory.\n{recovery}\n  - Retry the same operation with another workspace:\n    {command}",
        path.display()
    )
}

fn retry_command() -> String {
    let mut arguments = Vec::new();
    let mut source = std::env::args_os().skip(1);
    while let Some(argument) = source.next() {
        if argument == "--workspace" {
            let _ = source.next();
        } else if !argument.to_string_lossy().starts_with("--workspace=") {
            arguments.push(shell_display(&argument));
        }
    }
    let arguments = arguments.join(" ");
    if arguments.is_empty() {
        "mdtree --workspace /path/to/workspace.mdtree".into()
    } else {
        format!("mdtree --workspace /path/to/workspace.mdtree {arguments}")
    }
}

fn shell_display(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/".contains(character))
    {
        value.into_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn mutate_reference(
    store: &mut SqliteStore,
    source: &str,
    target: &str,
    relation: &str,
    expected: u64,
    add: bool,
) -> anyhow::Result<()> {
    let source_id = resolve_id(store, source)?;
    let target_id = resolve_id(store, target)?;
    let relation = ReferenceType::from_str(relation)?;
    let mut references = store
        .outgoing_references(source_id)?
        .into_iter()
        .filter(|item| item.origin == ReferenceOrigin::Explicit)
        .collect::<Vec<_>>();
    let matches = |item: &Reference| {
        item.reference_type == relation
            && matches!(item.target, ReferenceTarget::Resolved { node_id, .. } if node_id == target_id)
    };
    if add && !references.iter().any(matches) {
        references.push(Reference {
            source_node_id: source_id,
            source_section_id: None,
            reference_type: relation,
            target: ReferenceTarget::Resolved {
                node_id: target_id,
                target_ref: Some(target.into()),
                anchor: None,
            },
            origin: ReferenceOrigin::Explicit,
            metadata: std::collections::BTreeMap::new(),
        });
    } else if !add {
        references.retain(|item| !matches(item));
    }
    let current = store
        .get(source_id)?
        .ok_or_else(|| anyhow::anyhow!("source node not found"))?;
    let f = current.fields();
    let mut metadata = f.metadata.clone();
    metadata.extensions.insert(
        "explicit_relations".into(),
        serde_json::to_value(&references)?,
    );
    let prepared = prepare_cli_mutation(
        source_id,
        current.parent_id(),
        f.slug.clone(),
        metadata,
        f.markdown_content.clone(),
        f.sibling_order,
        expected + 1,
        f.created_at,
        now_millis()?,
        if add {
            "Add explicit reference"
        } else {
            "Remove explicit reference"
        },
    )?;
    store.set_explicit_references(prepared.change(expected), &references)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ChangeKind {
    Update,
    Rename,
    Move,
}

fn apply_change(
    store: &mut SqliteStore,
    prepared: &PreparedNodeMutation,
    expected: u64,
    kind: ChangeKind,
) -> anyhow::Result<mdtree_sqlite::MutationOutcome> {
    let change = prepared.change(expected);
    Ok(match kind {
        ChangeKind::Update => store.update_node(change)?,
        ChangeKind::Rename => store.rename_node(change)?,
        ChangeKind::Move => store.move_subtree(change)?,
    })
}

fn emit_mutation(
    output: &mut dyn Write,
    format: OutputFormat,
    outcome: mdtree_sqlite::MutationOutcome,
) -> anyhow::Result<()> {
    emit(
        output,
        format,
        &serde_json::json!({"status":format!("{outcome:?}").to_lowercase()}),
    )
}

fn current_node(store: &SqliteStore, selector: &str) -> anyhow::Result<Node> {
    let id = resolve_id(store, selector)?;
    store
        .get(id)?
        .ok_or_else(|| anyhow::anyhow!("node not found: {selector}"))
}

#[allow(clippy::too_many_lines)]
fn prepare_cli_batch(
    store: &SqliteStore,
    request: CliMutationBatch,
    now: u64,
) -> anyhow::Result<Vec<PreparedBatchOperation>> {
    let mut labels = BTreeMap::<String, NodeId>::new();
    let mut planned_slugs = BTreeMap::<NodeId, Vec<Slug>>::new();
    let mut operations = Vec::with_capacity(request.operations.len());
    for operation in request.operations {
        match operation {
            CliBatchOperation::Create {
                label,
                parent,
                title,
                content,
                requested_id,
                sibling_order,
            } => {
                let parent_id = batch_selector_id(store, &labels, &parent)?;
                let id = requested_id
                    .as_deref()
                    .map(NodeId::from_str)
                    .transpose()?
                    .unwrap_or_else(|| NodeId::new(SystemUlidGenerator.generate()));
                if let Some(label) = label {
                    if label.trim().is_empty() || labels.insert(label.clone(), id).is_some() {
                        anyhow::bail!("batch create label is blank or duplicated: {label}");
                    }
                }
                let existing = if store.get(parent_id)?.is_some() {
                    store.children(parent_id)?
                } else {
                    Vec::new()
                };
                let mut slugs = existing
                    .iter()
                    .map(|node| node.fields().slug.clone())
                    .collect::<Vec<_>>();
                slugs.extend(planned_slugs.get(&parent_id).cloned().unwrap_or_default());
                let slug = mdtree_core::generate_slug(&title, slugs.iter());
                planned_slugs
                    .entry(parent_id)
                    .or_default()
                    .push(slug.clone());
                let order = sibling_order.unwrap_or_else(|| {
                    u32::try_from(existing.len() + planned_slugs[&parent_id].len() - 1)
                        .unwrap_or(u32::MAX)
                });
                operations.push(PreparedBatchOperation::Create(prepare_cli_mutation(
                    id,
                    Some(parent_id),
                    slug,
                    NodeMetadata::new(&title),
                    content.unwrap_or_else(|| format!("# {title}\n")),
                    order,
                    1,
                    now,
                    now,
                    "Mutation batch create",
                )?));
            }
            CliBatchOperation::Update {
                selector,
                content,
                expected_version,
            } => {
                let current = batch_current_node(store, &labels, &selector)?;
                let f = current.fields();
                operations.push(PreparedBatchOperation::Replace {
                    prepared: prepare_cli_mutation(
                        current.id(),
                        current.parent_id(),
                        f.slug.clone(),
                        f.metadata.clone(),
                        content,
                        f.sibling_order,
                        expected_version + 1,
                        f.created_at,
                        now,
                        "Mutation batch update",
                    )?,
                    expected_version,
                });
            }
            CliBatchOperation::Rename {
                selector,
                title,
                expected_version,
            } => {
                let current = batch_current_node(store, &labels, &selector)?;
                let f = current.fields();
                let mut metadata = f.metadata.clone();
                metadata.title.clone_from(&title);
                let slug = current.parent_id().map_or_else(
                    || mdtree_core::generate_slug(&title, std::iter::empty::<&Slug>()),
                    |parent| {
                        store
                            .next_child_placement(parent, &title)
                            .map_or_else(|_| f.slug.clone(), |value| value.0)
                    },
                );
                operations.push(PreparedBatchOperation::Replace {
                    prepared: prepare_cli_mutation(
                        current.id(),
                        current.parent_id(),
                        slug,
                        metadata,
                        f.markdown_content.clone(),
                        f.sibling_order,
                        expected_version + 1,
                        f.created_at,
                        now,
                        "Mutation batch rename",
                    )?,
                    expected_version,
                });
            }
            CliBatchOperation::Move {
                selector,
                destination_parent,
                expected_version,
                sibling_order,
            } => {
                let current = batch_current_node(store, &labels, &selector)?;
                let parent = batch_selector_id(store, &labels, &destination_parent)?;
                let f = current.fields();
                let (slug, order) = store.next_child_placement(parent, &f.metadata.title)?;
                operations.push(PreparedBatchOperation::Replace {
                    prepared: prepare_cli_mutation(
                        current.id(),
                        Some(parent),
                        slug,
                        f.metadata.clone(),
                        f.markdown_content.clone(),
                        sibling_order.unwrap_or(order),
                        expected_version + 1,
                        f.created_at,
                        now,
                        "Mutation batch move",
                    )?,
                    expected_version,
                });
            }
            CliBatchOperation::Reorder {
                selector,
                sibling_order,
                expected_version,
            } => {
                let current = batch_current_node(store, &labels, &selector)?;
                let f = current.fields();
                operations.push(PreparedBatchOperation::Replace {
                    prepared: prepare_cli_mutation(
                        current.id(),
                        current.parent_id(),
                        f.slug.clone(),
                        f.metadata.clone(),
                        f.markdown_content.clone(),
                        sibling_order,
                        expected_version + 1,
                        f.created_at,
                        now,
                        "Mutation batch reorder",
                    )?,
                    expected_version,
                });
            }
            CliBatchOperation::Remove {
                selector,
                expected_version,
            } => {
                operations.push(PreparedBatchOperation::Remove(AtomicTreeRemoval {
                    node_id: batch_selector_id(store, &labels, &selector)?,
                    expected_version,
                }));
            }
            CliBatchOperation::SetReferences {
                selector,
                expected_version,
                references,
            } => {
                let current = batch_current_node(store, &labels, &selector)?;
                let f = current.fields();
                let explicit = references
                    .into_iter()
                    .map(|reference| {
                        Ok(Reference {
                            source_node_id: current.id(),
                            source_section_id: None,
                            reference_type: ReferenceType::from_str(&reference.relation)?,
                            target: ReferenceTarget::Resolved {
                                node_id: batch_selector_id(store, &labels, &reference.target)?,
                                target_ref: Some(reference.target),
                                anchor: None,
                            },
                            origin: ReferenceOrigin::Explicit,
                            metadata: BTreeMap::new(),
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let mut metadata = f.metadata.clone();
                metadata.extensions.insert(
                    "explicit_relations".into(),
                    serde_json::to_value(&explicit)?,
                );
                operations.push(PreparedBatchOperation::SetReferences {
                    prepared: prepare_cli_mutation(
                        current.id(),
                        current.parent_id(),
                        f.slug.clone(),
                        metadata,
                        f.markdown_content.clone(),
                        f.sibling_order,
                        expected_version + 1,
                        f.created_at,
                        now,
                        "Mutation batch references",
                    )?,
                    expected_version,
                    references: explicit,
                });
            }
        }
    }
    Ok(operations)
}

fn batch_selector_id(
    store: &SqliteStore,
    labels: &BTreeMap<String, NodeId>,
    selector: &str,
) -> anyhow::Result<NodeId> {
    labels
        .get(selector)
        .copied()
        .map_or_else(|| resolve_id(store, selector), Ok)
}

fn batch_current_node(
    store: &SqliteStore,
    labels: &BTreeMap<String, NodeId>,
    selector: &str,
) -> anyhow::Result<Node> {
    let id = batch_selector_id(store, labels, selector)?;
    store
        .get(id)?
        .ok_or_else(|| anyhow::anyhow!("batch operation requires an existing node: {selector}"))
}

fn now_millis() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

#[allow(clippy::too_many_arguments)]
fn prepare_cli_mutation(
    id: NodeId,
    parent_id: Option<NodeId>,
    slug: Slug,
    metadata: NodeMetadata,
    markdown_content: String,
    sibling_order: u32,
    version: u64,
    created_at: u64,
    updated_at: u64,
    change_summary: &str,
) -> anyhow::Result<PreparedNodeMutation> {
    Ok(prepare_node_mutation(
        NodeMutationDraft {
            id,
            parent_id,
            slug,
            metadata,
            markdown_content,
            sibling_order,
            version,
            created_at,
            updated_at,
            created_by: Some("cli".into()),
            change_summary: Some(change_summary.into()),
        },
        &SystemUlidGenerator,
    )?)
}

fn new_root(title: &str) -> anyhow::Result<Node> {
    let id = NodeId::new(SystemUlidGenerator.generate());
    let slug = mdtree_core::generate_slug(title, std::iter::empty::<&Slug>());
    let now = now_millis()?;
    Ok(prepare_cli_mutation(
        id,
        None,
        slug,
        NodeMetadata::new(title),
        format!("# {title}\n"),
        0,
        1,
        now,
        now,
        "Initialize workspace",
    )?
    .node)
}

fn required_workspace(cli: &Cli) -> &std::path::Path {
    cli.workspace
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new(".mdtree"))
}

fn resolve_id(store: &SqliteStore, selector: &str) -> anyhow::Result<NodeId> {
    let raw = selector;
    let selector = NodeSelector::from_str(raw)?;
    store
        .resolve(&selector)?
        .map(|node| node.id())
        .ok_or_else(|| anyhow::anyhow!("node not found: {raw}"))
}

fn parse_search_scope(value: &str) -> anyhow::Result<SearchScope> {
    match value {
        "current_node" => Ok(SearchScope::CurrentNode),
        "subtree" => Ok(SearchScope::Subtree),
        "siblings" => Ok(SearchScope::Siblings),
        "parent_subtree" => Ok(SearchScope::ParentSubtree),
        "workspace" => Ok(SearchScope::Workspace),
        "linked" => Ok(SearchScope::Linked),
        _ => anyhow::bail!("unknown search scope {value}"),
    }
}

fn validated_navigation_page(
    limit: Option<u32>,
    cursor: Option<&str>,
) -> Result<(PageLimit, Option<PageCursor>), PaginationError> {
    Ok((
        PageLimit::new(limit.unwrap_or(DEFAULT_PAGE_LIMIT))?,
        cursor.map(str::parse).transpose()?,
    ))
}

fn resolve_projection(
    store: &SqliteStore,
    selector: &str,
) -> anyhow::Result<mdtree_core::NodeProjection> {
    let raw = selector;
    let selector = NodeSelector::from_str(raw)?;
    store
        .resolve_projection(&selector)?
        .ok_or_else(|| anyhow::anyhow!("node not found: {raw}"))
}

fn emit_depths(
    output: &mut dyn Write,
    format: OutputFormat,
    store: &SqliteStore,
    depths: Vec<mdtree_sqlite::NodeDepth>,
) -> anyhow::Result<()> {
    let values = store
        .project_depths(depths)
        .into_iter()
        .map(|item| serde_json::json!({"depth":item.depth,"node":item.node}))
        .collect::<Vec<_>>();
    emit(output, format, &values)
}

fn browse_rows(
    store: &SqliteStore,
    root: NodeId,
) -> anyhow::Result<Vec<(u32, mdtree_core::SnapshotNode)>> {
    Ok(store
        .project_depths(store.subtree(root)?)
        .into_iter()
        .map(|item| (item.depth, item.node))
        .collect())
}

fn browse_line_mode(
    store: &SqliteStore,
    root: NodeId,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> anyhow::Result<()> {
    let rows = browse_rows(store, root)?;
    loop {
        write!(output, "\x1b[2J\x1b[HMDTree browser\n==============\n\n")?;
        for (index, (depth, node)) in rows.iter().enumerate() {
            let branch = if *depth == 0 { "● " } else { "└─ " };
            writeln!(
                output,
                "{:>3}) {}{}{}  [slug: {}, v{}]",
                index + 1,
                "  ".repeat(usize::try_from(*depth)?),
                branch,
                node.metadata.title,
                node.slug,
                node.version
            )?;
        }
        write!(output, "\n  q) Quit\n\nSelect node: ")?;
        output.flush()?;
        let mut choice = String::new();
        if input.read_line(&mut choice)? == 0 {
            break;
        }
        let choice = choice.trim();
        if matches!(choice, "q" | "Q" | "quit" | "exit") {
            break;
        }
        let Ok(number) = choice.parse::<usize>() else {
            writeln!(output, "Invalid selection. Press Enter to continue.")?;
            let mut ignored = String::new();
            input.read_line(&mut ignored)?;
            continue;
        };
        let Some((depth, node)) = number.checked_sub(1).and_then(|index| rows.get(index)) else {
            writeln!(output, "Selection out of range. Press Enter to continue.")?;
            let mut ignored = String::new();
            input.read_line(&mut ignored)?;
            continue;
        };
        write!(
            output,
            "\x1b[2J\x1b[HTitle:   {}\nSlug:    {}\nID:      {}\nParent:  {}\nDepth:   {}\nVersion: {}\n\nMarkdown:\n---------\n{}\n\nPress Enter to return to the tree.",
            node.metadata.title,
            node.slug,
            node.id,
            node.parent_id.map_or_else(|| "-".into(), |id| id.to_string()),
            depth,
            node.version,
            node.markdown_content
        )?;
        output.flush()?;
        let mut ignored = String::new();
        if input.read_line(&mut ignored)? == 0 {
            break;
        }
    }
    Ok(())
}

struct RawTerminalGuard;

impl RawTerminalGuard {
    fn enter(output: &mut dyn Write) -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        write!(output, "\x1b[?1049h\x1b[?25l")?;
        output.flush()?;
        Ok(Self)
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = write!(io::stdout(), "\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}

fn browse_interactive(
    store: &SqliteStore,
    root: NodeId,
    output: &mut dyn Write,
) -> anyhow::Result<()> {
    let rows = browse_rows(store, root)?;
    let _guard = RawTerminalGuard::enter(output)?;
    let mut selected = 0_usize;
    let mut detail = false;
    let mut detail_scroll = 0_usize;
    loop {
        let (_, height) = crossterm::terminal::size().unwrap_or((80, 24));
        let mut detail_page = 1_usize;
        let mut detail_max_scroll = 0_usize;
        write!(output, "\x1b[2J\x1b[H")?;
        if detail {
            let (depth, node) = &rows[selected];
            detail_page = usize::from(height.saturating_sub(11)).max(1);
            let (clamped_scroll, maximum, visible_lines) =
                detail_window(&node.markdown_content, detail_scroll, detail_page);
            detail_scroll = clamped_scroll;
            detail_max_scroll = maximum;
            write!(
                output,
                "Title:   {}\r\nSlug:    {}\r\nID:      {}\r\nParent:  {}\r\nDepth:   {}\r\nVersion: {}\r\n\r\nMarkdown:\r\n---------\r\n",
                node.metadata.title,
                node.slug,
                node.id,
                node.parent_id.map_or_else(|| "-".into(), |id| id.to_string()),
                depth,
                node.version,
            )?;
            for line in &visible_lines {
                write!(output, "{line}\r\n")?;
            }
            for _ in visible_lines.len()..detail_page {
                write!(output, "\r\n")?;
            }
            write!(
                output,
                "\r\n↑/↓: scroll  PgUp/PgDn: page  g/G: top/end  ←/Esc: tree  q: quit"
            )?;
        } else {
            writeln!(output, "MDTree browser\r")?;
            writeln!(output, "==============\r\n")?;
            let visible = usize::from(height.saturating_sub(7)).max(1);
            let start = selected
                .saturating_sub(visible / 2)
                .min(rows.len().saturating_sub(visible));
            for (index, (depth, node)) in rows.iter().enumerate().skip(start).take(visible) {
                let marker = if index == selected { "▶" } else { " " };
                let branch = if *depth == 0 { "● " } else { "└─ " };
                if index == selected {
                    write!(output, "\x1b[7m")?;
                }
                write!(
                    output,
                    "{marker} {}{branch}{}  [slug: {}, v{}]\x1b[0m\r\n",
                    "  ".repeat(usize::try_from(*depth)?),
                    node.metadata.title,
                    node.slug,
                    node.version
                )?;
            }
            write!(output, "\r\n↑/↓: select  Enter: open  q: quit")?;
        }
        output.flush()?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if matches!(key.code, KeyCode::Char('q' | 'Q')) {
            break;
        }
        if detail {
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Enter => detail = false,
                _ => {
                    detail_scroll = detail_scroll_offset(
                        key.code,
                        detail_scroll,
                        detail_page,
                        detail_max_scroll,
                    );
                }
            }
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                detail = true;
                detail_scroll = 0;
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(rows.len() - 1),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = rows.len() - 1,
            _ => {}
        }
    }
    Ok(())
}

fn detail_window(markdown: &str, requested: usize, page: usize) -> (usize, usize, Vec<&str>) {
    let lines = markdown.split('\n').collect::<Vec<_>>();
    let maximum = lines.len().saturating_sub(page);
    let scroll = requested.min(maximum);
    let visible = lines.into_iter().skip(scroll).take(page).collect();
    (scroll, maximum, visible)
}

fn detail_scroll_offset(key: KeyCode, current: usize, page: usize, maximum: usize) -> usize {
    match key {
        KeyCode::Up => current.saturating_sub(1),
        KeyCode::Down => (current + 1).min(maximum),
        KeyCode::PageUp => current.saturating_sub(page),
        KeyCode::PageDown => current.saturating_add(page).min(maximum),
        KeyCode::Char('g') | KeyCode::Home => 0,
        KeyCode::Char('G') | KeyCode::End => maximum,
        _ => current,
    }
}

fn emit_ok(output: &mut dyn Write, format: OutputFormat, status: &str) -> anyhow::Result<u8> {
    emit(output, format, &serde_json::json!({"status":status}))?;
    Ok(EXIT_OK)
}

fn emit<T: Serialize + ?Sized>(
    output: &mut dyn Write,
    format: OutputFormat,
    value: &T,
) -> anyhow::Result<()> {
    match format {
        OutputFormat::Text | OutputFormat::Json => {
            serde_json::to_writer_pretty(&mut *output, value)?;
            writeln!(output)?;
        }
        OutputFormat::Jsonl => {
            let value = serde_json::to_value(value)?;
            if let serde_json::Value::Array(items) = value {
                for item in items {
                    serde_json::to_writer(&mut *output, &item)?;
                    writeln!(output)?;
                }
            } else {
                serde_json::to_writer(&mut *output, &value)?;
                writeln!(output)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;

    use clap::{CommandFactory, Parser};
    use tempfile::tempdir;

    use super::{
        browse_line_mode, detail_scroll_offset, detail_window, emit, execute,
        export_complete_workspace_snapshot, Cli, Command, NavigationRelation, OutputFormat,
        PaginationArgs, SnapshotFormat, EXIT_OK,
    };

    #[test]
    fn cli_pagination_flags_use_the_shared_opaque_contract() {
        #[derive(clap::Parser)]
        struct PaginationCli {
            #[command(flatten)]
            pagination: PaginationArgs,
        }

        let root = "01JZ8Q5CWPN8T7KPN5A1V9B6XM"
            .parse::<mdtree_core::NodeId>()
            .expect("root ID");
        let item = "01JZ8Q5CWPN8T7KPN5A1V9B6XN"
            .parse::<mdtree_core::NodeId>()
            .expect("item ID");
        let scope =
            mdtree_core::CursorScope::new("children", Some(root), "parent=root").expect("scope");
        let cursor = mdtree_core::PageCursor::issue(
            9,
            scope.clone(),
            mdtree_core::PagePosition::Sibling {
                sibling_order: 1,
                node_id: item,
            },
        )
        .expect("cursor");
        let args =
            PaginationCli::try_parse_from(["page", "--limit", "2", "--cursor", cursor.as_str()])
                .expect("CLI flags");
        let (limit, parsed) = args.pagination.validated().expect("validated flags");
        assert_eq!(limit.get(), 2);
        assert_eq!(
            parsed.expect("cursor").resume(&scope, 9),
            Ok(mdtree_core::PagePosition::Sibling {
                sibling_order: 1,
                node_id: item,
            })
        );
        assert_eq!(
            PaginationArgs {
                limit: 0,
                cursor: None
            }
            .validated()
            .expect_err("limit"),
            mdtree_core::PaginationError::InvalidLimit {
                requested: 0,
                minimum: 1,
                maximum: 100,
            }
        );
    }

    fn assert_canonical_nodes(value: &serde_json::Value, expected: &[mdtree_core::NodeId]) {
        let nodes = value.as_array().expect("node array");
        assert_eq!(nodes.len(), expected.len());
        for (node, expected_id) in nodes.iter().zip(expected) {
            assert_eq!(node["id"], serde_json::json!(expected_id));
        }
        assert!(nodes.windows(2).all(|pair| {
            let left = (
                pair[0]["sibling_order"].as_u64().expect("left order"),
                pair[0]["id"].as_str().expect("left id"),
            );
            let right = (
                pair[1]["sibling_order"].as_u64().expect("right order"),
                pair[1]["id"].as_str().expect("right id"),
            );
            left < right
        }));
        assert!(nodes.windows(2).any(|pair| {
            pair[0]["id"].as_str().expect("left id") > pair[1]["id"].as_str().expect("right id")
        }));
    }

    fn deep_fixture_ids(fixture: &mdtree_core::LargeTreeFixture) -> Vec<mdtree_core::NodeId> {
        fixture
            .snapshot
            .nodes
            .iter()
            .filter(|node| {
                node.id == fixture.deep_parent_id || node.slug.as_str().starts_with("deep-")
            })
            .map(|node| node.id)
            .collect()
    }

    #[test]
    fn help_and_command_contract_are_complete_and_stable() {
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        for name in [
            "completions",
            "init",
            "status",
            "backup",
            "restore",
            "export",
            "export-node",
            "import",
            "rebuild-indexes",
            "prune-history",
            "check",
            "validate",
            "doctor",
            "root",
            "tree",
            "browse",
            "browse-ui",
            "show",
            "batch-node",
            "batch-children",
            "statistics",
            "contains",
            "lowest-common-ancestor",
            "path-between",
            "distance",
            "filter",
            "child-at",
            "adjacent-siblings",
            "children",
            "parent",
            "ancestors",
            "descendants",
            "siblings",
            "subtree",
            "navigate",
            "path",
            "search",
            "locate",
            "inspect",
            "examples",
            "context",
            "create",
            "update",
            "rename",
            "move",
            "clone-subtree",
            "atomic-tree-batch",
            "mutation-batch",
            "remove",
            "history",
            "revision",
            "diff",
            "subtree-diff",
            "restore-version",
            "references",
            "backlinks",
            "reference-add",
            "reference-remove",
            "unresolved",
            "duplicates",
        ] {
            assert!(help.contains(name), "missing {name} from help");
        }
        assert!(help.contains("complete whole-workspace interoperable snapshot"));

        assert!(Cli::try_parse_from(["mdtree", "prune-history"]).is_err());
        assert!(Cli::try_parse_from(["mdtree", "prune-history", "--dry-run"]).is_ok());
        assert!(Cli::try_parse_from(["mdtree", "prune-history", "--yes"]).is_ok());
        assert!(Cli::try_parse_from(["mdtree", "prune-history", "--vacuum"]).is_err());

        let clone_help = command
            .find_subcommand_mut("clone-subtree")
            .expect("clone-subtree command")
            .render_long_help()
            .to_string();
        for contract in [
            "Existing subtree root selected by ID, slug, or canonical path",
            "Version observed on the source before cloning",
            "Validate and report the clone plan without writing",
            "Reason recorded on every immutable clone revision",
        ] {
            assert!(clone_help.contains(contract), "clone help lacks {contract}");
        }

        let mut command = Cli::command();
        let batch_help = command
            .find_subcommand_mut("mutation-batch")
            .expect("mutation-batch command")
            .render_long_help()
            .to_string();
        for contract in [
            "1 through 50 ordered operations",
            "set_references",
            "labels that later operations use as selectors",
            "rolls back the complete batch",
        ] {
            assert!(batch_help.contains(contract), "batch help lacks {contract}");
        }
    }

    #[test]
    fn complete_export_is_confined_to_the_explicit_cli_export_command() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("export-boundary.mdtree");
        let destination = directory.path().join("complete.snapshot.json");
        let fixture = mdtree_core::generate_large_tree_fixture(
            mdtree_core::LargeTreeFixtureSpec {
                wide_children: 12,
                deep_descendants: 8,
                history_revisions: 6,
                relations: 10,
                response_boundary_bytes: 4096,
            },
            943,
        );
        mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
        let (store, observer) =
            mdtree_sqlite::test_support::open_observed_store(&workspace).expect("observed store");
        export_complete_workspace_snapshot(&store, &destination, SnapshotFormat::Json)
            .expect("complete export");
        assert!(observer.observation().has_complete_snapshot_signature());

        let production = include_str!("lib.rs")
            .split_once("#[cfg(test)]")
            .expect("test module boundary")
            .0;
        assert_eq!(
            production
                .matches("export_complete_workspace_snapshot(&store")
                .count(),
            1,
            "only Command::Export may invoke complete snapshot serialization"
        );
        assert_eq!(production.matches("export_snapshot_json(store)").count(), 1);
    }

    #[test]
    fn completions_are_generated_without_a_workspace() {
        let cli =
            Cli::try_parse_from(["mdtree", "completions", "bash"]).expect("completion command");
        let mut output = Vec::new();
        assert_eq!(
            execute(&cli, &mut output).expect("generate completions"),
            EXIT_OK
        );
        let script = String::from_utf8(output).expect("UTF-8 completion script");
        assert!(script.contains("mdtree"));
        assert!(script.contains("completions"));
        assert!(script.contains("--workspace"));
    }

    #[test]
    fn workspace_defaults_to_dot_mdtree_and_explicit_path_wins() {
        let default = Cli::try_parse_from(["mdtree", "status"]).expect("default CLI");
        assert_eq!(default.workspace, None);
        assert_eq!(super::required_workspace(&default), Path::new(".mdtree"));

        let explicit = Cli::try_parse_from(["mdtree", "--workspace", "custom.mdtree", "status"])
            .expect("explicit CLI");
        assert_eq!(
            explicit.workspace.as_deref(),
            Some(Path::new("custom.mdtree"))
        );
    }

    #[test]
    fn export_node_requires_subtree_when_depth_is_given() {
        let parsed = Cli::try_parse_from([
            "mdtree",
            "export-node",
            "architecture",
            "/tmp",
            "--depth",
            "2",
        ]);
        assert!(parsed.is_err());
        assert!(Cli::try_parse_from([
            "mdtree",
            "export-node",
            "architecture",
            "/tmp",
            "--subtree",
            "--depth",
            "2",
        ])
        .is_ok());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn export_node_writes_root_first_and_preserves_bounded_tree_depth() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("northstar.mdtree");
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/northstar-platform.snapshot.json");
        assert_eq!(
            run(
                &workspace,
                Command::Import {
                    source: fixture,
                    format: super::SnapshotFormat::Json,
                },
            )
            .0,
            EXIT_OK
        );

        let single = directory.path().join("architecture-export.md");
        let (code, single_result) = run(
            &workspace,
            Command::ExportNode {
                selector: "architecture".into(),
                destination: single.clone(),
                subtree: false,
                depth: None,
            },
        );
        assert_eq!(code, EXIT_OK);
        let single_files = single_result["exported_files"]
            .as_array()
            .expect("exported files");
        assert_eq!(single_result.as_object().expect("result").len(), 1);
        assert_eq!(single_files.len(), 1);
        assert_eq!(single_files[0], single.to_string_lossy().as_ref());
        assert!(single.is_file());
        assert!(!directory.path().join("architecture-decisions").exists());

        let bounded = directory.path().join("bounded");
        std::fs::create_dir(&bounded).expect("existing output directory");
        std::fs::write(bounded.join("unrelated.txt"), "preserved").expect("unrelated file");
        let bounded_child = bounded.join("architecture-decisions");
        std::fs::create_dir(&bounded_child).expect("existing descendant directory");
        std::fs::write(bounded.join("architecture.md"), "stale root").expect("stale root");
        std::fs::write(
            bounded_child.join("architecture-decisions.md"),
            "stale child",
        )
        .expect("stale child");
        let (code, bounded_result) = run(
            &workspace,
            Command::ExportNode {
                selector: "/architecture/".into(),
                destination: bounded.clone(),
                subtree: true,
                depth: Some(1),
            },
        );
        assert_eq!(code, EXIT_OK);
        let bounded_files = bounded_result["exported_files"]
            .as_array()
            .expect("exported files");
        assert_eq!(bounded_result.as_object().expect("result").len(), 1);
        assert_eq!(bounded_files.len(), 2);
        assert_eq!(
            bounded_files[0],
            bounded.join("architecture.md").to_string_lossy().as_ref()
        );
        assert!(bounded.join("architecture.md").is_file());
        assert!(bounded
            .join("architecture-decisions/architecture-decisions.md")
            .is_file());
        assert!(!bounded
            .join("architecture-decisions/postgresql-as-system-of-record")
            .exists());
        assert_eq!(
            std::fs::read_to_string(bounded.join("unrelated.txt")).expect("unrelated file"),
            "preserved"
        );

        let complete_directory = directory.path().join("complete");
        let complete = complete_directory.join("architecture-export.md");
        let (code, complete_result) = run(
            &workspace,
            Command::ExportNode {
                selector: "architecture".into(),
                destination: complete.clone(),
                subtree: true,
                depth: None,
            },
        );
        assert_eq!(code, EXIT_OK);
        let complete_files = complete_result["exported_files"]
            .as_array()
            .expect("exported files");
        assert_eq!(complete_result.as_object().expect("result").len(), 1);
        assert_eq!(complete_files.len(), 5);
        assert_eq!(complete_files[0], complete.to_string_lossy().as_ref());
        let adr = complete_directory.join(
            "architecture-decisions/postgresql-as-system-of-record/postgresql-as-system-of-record.md",
        );
        assert!(adr.is_file());
        let root = std::fs::read_to_string(&complete).expect("exported root Markdown");
        assert!(root.starts_with("---\n"));
        assert!(root.contains("slug: architecture"));
        assert!(root.contains("# Architecture"));

        std::fs::write(&complete, "stale root").expect("stale root");
        std::fs::write(&adr, "stale descendant").expect("stale descendant");
        let (code, repeated) = run(
            &workspace,
            Command::ExportNode {
                selector: "architecture".into(),
                destination: complete.clone(),
                subtree: true,
                depth: None,
            },
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            repeated["exported_files"]
                .as_array()
                .expect("exported files")
                .len(),
            5
        );
        assert!(std::fs::read_to_string(&complete)
            .expect("overwritten root")
            .contains("# Architecture"));
        assert!(std::fs::read_to_string(&adr)
            .expect("overwritten descendant")
            .contains("# ADR-001"));
    }

    #[test]
    fn text_json_and_jsonl_serializers_are_equivalent() {
        let value = serde_json::json!([{"id":1,"title":"A"},{"id":2,"title":"B"}]);
        let mut text = Vec::new();
        let mut json = Vec::new();
        let mut jsonl = Vec::new();
        emit(&mut text, OutputFormat::Text, &value).expect("text");
        emit(&mut json, OutputFormat::Json, &value).expect("JSON");
        emit(&mut jsonl, OutputFormat::Jsonl, &value).expect("JSONL");
        assert_eq!(text, json);
        let lines = String::from_utf8(jsonl).expect("UTF-8");
        let items = lines
            .lines()
            .map(|line| serde_json::from_str(line).expect("item"))
            .collect::<Vec<serde_json::Value>>();
        assert_eq!(items, value.as_array().expect("array").clone());
    }

    #[test]
    fn text_and_json_stream_large_values_without_an_adapter_byte_rejection() {
        #[derive(Default)]
        struct CountingWriter(usize);

        impl Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let value = serde_json::json!({"content":"x".repeat(1_100_000)});
        for format in [OutputFormat::Text, OutputFormat::Json] {
            let mut output = CountingWriter::default();
            emit(&mut output, format, &value).expect("streamed output");
            assert!(output.0 > 1_048_576);
        }
    }

    #[test]
    fn browser_detail_view_starts_at_top_and_supports_requested_scroll_keys() {
        use crossterm::event::KeyCode;

        let markdown = (0..30)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (scroll, maximum, visible) = detail_window(&markdown, 0, 5);
        assert_eq!(scroll, 0);
        assert_eq!(maximum, 25);
        assert_eq!(visible, ["line 0", "line 1", "line 2", "line 3", "line 4"]);

        assert_eq!(detail_scroll_offset(KeyCode::Up, 0, 5, maximum), 0);
        assert_eq!(detail_scroll_offset(KeyCode::Down, 0, 5, maximum), 1);
        assert_eq!(detail_scroll_offset(KeyCode::PageDown, 3, 5, maximum), 8);
        assert_eq!(detail_scroll_offset(KeyCode::PageUp, 3, 5, maximum), 0);
        assert_eq!(detail_scroll_offset(KeyCode::Char('g'), 9, 5, maximum), 0);
        assert_eq!(
            detail_scroll_offset(KeyCode::Char('G'), 9, 5, maximum),
            maximum
        );

        let (scroll, _, visible) = detail_window(&markdown, usize::MAX, 5);
        assert_eq!(scroll, maximum);
        assert_eq!(visible.first(), Some(&"line 25"));
        assert_eq!(visible.last(), Some(&"line 29"));
    }

    #[test]
    fn browser_lists_nodes_and_opens_markdown_without_external_tools() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("northstar.mdtree");
        mdtree_sqlite::import_snapshot_new(&workspace, &mdtree_core::northstar_platform_snapshot())
            .expect("import");
        let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("workspace");
        let root = store.root().expect("root").id();
        let mut input = std::io::Cursor::new(b"2\n\nq\n");
        let mut output = Vec::new();
        browse_line_mode(&store, root, &mut input, &mut output).expect("browse");
        let rendered = String::from_utf8(output).expect("UTF-8");
        assert!(rendered.contains("MDTree browser"));
        assert!(rendered.contains("Architecture Decisions"));
        assert!(rendered.contains("Title:   Architecture"));
        assert!(rendered.contains("# Architecture"));
    }

    #[test]
    fn cli_reuses_composite_scale_fixture_for_wide_and_bounded_reads() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("scale.mdtree");
        let spec = mdtree_core::LargeTreeFixtureSpec {
            wide_children: 120,
            deep_descendants: 24,
            history_revisions: 12,
            relations: 48,
            response_boundary_bytes: 4096,
        };
        let fixture = mdtree_core::generate_large_tree_fixture(spec, 91);
        mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("scale import");

        let children = run(
            &workspace,
            Command::Children {
                selector: fixture.wide_parent_id.to_string(),
                pagination: PaginationArgs {
                    limit: 100,
                    cursor: None,
                },
            },
        )
        .1;
        assert_canonical_nodes(&children["items"], &fixture.wide_child_ids[..100]);
        assert!(children["next_cursor"].is_string());

        let siblings = run(
            &workspace,
            Command::Siblings {
                selector: fixture.wide_child_ids[73].to_string(),
                pagination: PaginationArgs {
                    limit: 100,
                    cursor: None,
                },
            },
        )
        .1;
        assert_canonical_nodes(&siblings["items"], &fixture.wide_child_ids[..100]);

        let deep_ids = deep_fixture_ids(&fixture);
        let ancestors = run(
            &workspace,
            Command::Ancestors {
                selector: fixture.deep_leaf_id.to_string(),
            },
        )
        .1;
        let ancestor_ids = std::iter::once(fixture.root_id)
            .chain(deep_ids[..deep_ids.len() - 1].iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(
            ancestors.as_array().expect("ancestors").len(),
            ancestor_ids.len()
        );
        for (row, expected_id) in ancestors
            .as_array()
            .expect("ancestors")
            .iter()
            .zip(ancestor_ids)
        {
            assert_eq!(row["node"]["id"], serde_json::json!(expected_id));
        }

        let subtree = run(
            &workspace,
            Command::Subtree {
                selector: fixture.deep_parent_id.to_string(),
                order: crate::TraversalOrder::Dfs,
                pagination: PaginationArgs {
                    limit: 50,
                    cursor: None,
                },
            },
        )
        .1;
        for (depth, (row, expected_id)) in subtree["items"]
            .as_array()
            .expect("subtree")
            .iter()
            .zip(&deep_ids)
            .enumerate()
        {
            assert_eq!(row["depth"], depth);
            assert_eq!(row["node"]["id"], serde_json::json!(expected_id));
        }
        let inspection = run(
            &workspace,
            Command::Inspect {
                selector: fixture.wide_parent_id.to_string(),
                depth: 1,
                pagination: PaginationArgs {
                    limit: 10,
                    cursor: None,
                },
            },
        )
        .1;
        assert_eq!(inspection["items"].as_array().expect("items").len(), 10);
        assert_eq!(inspection["items"][0]["depth"], 0);
        assert_eq!(inspection["items"][0]["child_count"], 120);
        assert_eq!(inspection["items"][1]["depth"], 1);
        assert_eq!(inspection["items"][1]["child_count"], 0);
        assert_eq!(inspection["truncated"], true);
    }

    #[test]
    fn cli_node_and_depth_output_preserve_shared_projection_contracts() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("projection.mdtree");
        let fixture = mdtree_core::generate_large_tree_fixture(
            mdtree_core::LargeTreeFixtureSpec {
                wide_children: 8,
                deep_descendants: 5,
                history_revisions: 4,
                relations: 6,
                response_boundary_bytes: 4096,
            },
            93,
        );
        mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("scale import");
        let selected = fixture.wide_child_ids[3];
        let expected = fixture
            .snapshot
            .nodes
            .iter()
            .find(|node| node.id == selected)
            .expect("fixture node");

        let shown = run(
            &workspace,
            Command::Show {
                selector: selected.to_string(),
            },
        )
        .1;
        assert_eq!(
            shown[0],
            serde_json::to_value(expected).expect("expected JSON")
        );

        let descendants = run(
            &workspace,
            Command::Descendants {
                selector: fixture.deep_parent_id.to_string(),
                order: crate::TraversalOrder::Dfs,
                pagination: PaginationArgs {
                    limit: 50,
                    cursor: None,
                },
            },
        )
        .1;
        assert_eq!(descendants["items"][0]["depth"], 1);
        assert_eq!(
            descendants["items"][0]["node"]["parent_id"],
            serde_json::json!(fixture.deep_parent_id)
        );
        assert!(descendants["items"][0]["node"]
            .get("revision_hash")
            .is_some());
    }

    #[test]
    fn cli_root_node_selectors_and_parent_preserve_single_read_contracts() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("single-node.mdtree");
        let fixture = mdtree_core::generate_large_tree_fixture(
            mdtree_core::LargeTreeFixtureSpec {
                wide_children: 64,
                deep_descendants: 12,
                history_revisions: 10,
                relations: 24,
                response_boundary_bytes: 4096,
            },
            96,
        );
        mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("scale import");
        let expected = fixture
            .snapshot
            .nodes
            .iter()
            .find(|node| node.id == fixture.history_node_id)
            .expect("history fixture node");

        assert_eq!(
            run(&workspace, Command::Root).1["id"],
            serde_json::json!(fixture.root_id)
        );
        for selector in [
            fixture.history_node_id.to_string(),
            "history-heavy".into(),
            "/history-heavy".into(),
        ] {
            let shown = run(&workspace, Command::Show { selector }).1;
            assert_eq!(
                shown[0],
                serde_json::to_value(expected).expect("expected projection")
            );
        }

        let parent = run(
            &workspace,
            Command::Parent {
                selector: fixture.history_node_id.to_string(),
            },
        )
        .1;
        assert_eq!(parent.as_array().expect("parent").len(), 1);
        assert_eq!(parent[0]["id"], serde_json::json!(fixture.root_id));
        let root_parent = run(
            &workspace,
            Command::Parent {
                selector: fixture.root_id.to_string(),
            },
        )
        .1;
        assert!(root_parent.as_array().expect("root parent").is_empty());
    }

    fn run(workspace: &std::path::Path, command: Command) -> (u8, serde_json::Value) {
        let mut output = Vec::new();
        let code = execute(
            &Cli {
                workspace: Some(workspace.to_path_buf()),
                workspace_name: None,
                output: OutputFormat::Json,
                no_open: false,
                port: 0,
                command: Some(command),
            },
            &mut output,
        )
        .expect("CLI execution");
        let value = serde_json::from_slice(&output).expect("JSON output");
        (code, value)
    }

    #[test]
    fn navigate_selects_every_relation_and_preserves_legacy_inspection() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("navigate.mdtree");
        let fixture = mdtree_core::generate_large_tree_fixture(
            mdtree_core::LargeTreeFixtureSpec {
                wide_children: 8,
                deep_descendants: 3,
                history_revisions: 1,
                relations: 0,
                response_boundary_bytes: 4096,
            },
            918,
        );
        mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
        for relation in [
            NavigationRelation::Parent,
            NavigationRelation::Children,
            NavigationRelation::Ancestors,
            NavigationRelation::Descendants,
            NavigationRelation::Siblings,
            NavigationRelation::Subtree,
        ] {
            let unpaginated = matches!(
                relation,
                NavigationRelation::Parent | NavigationRelation::Ancestors
            );
            let value = run(
                &workspace,
                Command::Navigate {
                    selector: fixture.wide_parent_id.to_string(),
                    relation: Some(relation),
                    depth: None,
                    limit: (!unpaginated).then_some(2),
                    cursor: None,
                },
            )
            .1;
            if unpaginated {
                assert!(value.is_array(), "{relation:?}");
            } else {
                assert!(value["items"].is_array(), "{relation:?}");
            }
        }
        let legacy = run(
            &workspace,
            Command::Navigate {
                selector: fixture.wide_parent_id.to_string(),
                relation: None,
                depth: Some(1),
                limit: Some(2),
                cursor: None,
            },
        )
        .1;
        assert_eq!(
            legacy["items"][0]["node"]["node_id"],
            fixture.wide_parent_id.to_string()
        );

        for command in [
            Command::Navigate {
                selector: fixture.wide_parent_id.to_string(),
                relation: Some(NavigationRelation::Children),
                depth: Some(1),
                limit: None,
                cursor: None,
            },
            Command::Navigate {
                selector: fixture.wide_parent_id.to_string(),
                relation: Some(NavigationRelation::Parent),
                depth: None,
                limit: Some(2),
                cursor: None,
            },
        ] {
            let error = execute(
                &Cli {
                    workspace: Some(workspace.clone()),
                    workspace_name: None,
                    output: OutputFormat::Json,
                    no_open: false,
                    port: 0,
                    command: Some(command),
                },
                &mut Vec::new(),
            )
            .expect_err("invalid navigate options");
            assert!(error.to_string().contains("supported"));
        }
        assert!(
            Cli::try_parse_from(["mdtree", "navigate", "wide", "--relation", "sideways"]).is_err()
        );
    }

    fn run_code(workspace: &std::path::Path, command: Command) -> u8 {
        execute(
            &Cli {
                workspace: Some(workspace.to_path_buf()),
                workspace_name: None,
                output: OutputFormat::Json,
                no_open: false,
                port: 0,
                command: Some(command),
            },
            &mut Vec::new(),
        )
        .expect("CLI execution")
    }

    #[test]
    fn complete_workspace_lifecycle_commands_execute_end_to_end() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("source.mdtree");
        assert_eq!(
            run(
                &workspace,
                Command::Init {
                    name: "Northstar Platform".into()
                }
            )
            .0,
            EXIT_OK
        );
        assert_eq!(run(&workspace, Command::Status).0, EXIT_OK);
        assert_eq!(run(&workspace, Command::RebuildIndexes).0, EXIT_OK);
        assert_eq!(run(&workspace, Command::Doctor).0, EXIT_OK);
        let json = directory.path().join("snapshot.json");
        assert_eq!(
            run_code(
                &workspace,
                Command::Export {
                    destination: json.clone(),
                    format: super::SnapshotFormat::Json
                }
            ),
            EXIT_OK
        );
        let markdown = directory.path().join("snapshot-md");
        assert_eq!(
            run_code(
                &workspace,
                Command::Export {
                    destination: markdown.clone(),
                    format: super::SnapshotFormat::Markdown
                }
            ),
            EXIT_OK
        );
        let backup = directory.path().join("backup.mdtree");
        assert_eq!(
            run(
                &workspace,
                Command::Backup {
                    destination: backup.clone()
                }
            )
            .0,
            EXIT_OK
        );
        let restored = directory.path().join("restored.mdtree");
        assert_eq!(
            run(
                &restored,
                Command::Restore {
                    source: backup,
                    overwrite: false
                }
            )
            .0,
            EXIT_OK
        );
        assert_eq!(run(&restored, Command::Check).0, EXIT_OK);
        let imported_json = directory.path().join("json-import.mdtree");
        assert_eq!(
            run(
                &imported_json,
                Command::Import {
                    source: json,
                    format: super::SnapshotFormat::Json
                }
            )
            .0,
            EXIT_OK
        );
        let imported_markdown = directory.path().join("md-import.mdtree");
        assert_eq!(
            run(
                &imported_markdown,
                Command::Import {
                    source: markdown,
                    format: super::SnapshotFormat::Markdown
                }
            )
            .0,
            EXIT_OK
        );
    }

    #[test]
    fn cli_prune_history_previews_then_retains_only_current_heads() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("prune-history.mdtree");
        assert_eq!(
            run(
                &workspace,
                Command::Init {
                    name: "Prune History".into(),
                },
            )
            .0,
            EXIT_OK
        );
        let root = run(&workspace, Command::Root).1["id"]
            .as_str()
            .expect("root ID")
            .to_owned();
        let child = run(
            &workspace,
            Command::Create {
                parent: root,
                title: "Child".into(),
                content: None,
                dry_run: false,
            },
        )
        .1["node_id"]
            .as_str()
            .expect("child ID")
            .to_owned();
        for (expected_version, content) in [(1, "# Child\nTwo"), (2, "# Child\nThree")] {
            assert_eq!(
                run(
                    &workspace,
                    Command::Update {
                        selector: child.clone(),
                        content: content.into(),
                        expected_version,
                        dry_run: false,
                    },
                )
                .1["status"],
                "applied"
            );
        }

        let planned = run(
            &workspace,
            Command::PruneHistory {
                dry_run: true,
                yes: false,
                vacuum: false,
            },
        )
        .1;
        assert_eq!(planned["status"], "planned");
        assert_eq!(planned["nodes"], 2);
        assert_eq!(planned["revisions_before"], 4);
        assert_eq!(planned["revisions_removed"], 2);
        assert_eq!(
            run(
                &workspace,
                Command::History {
                    selector: child.clone(),
                    pagination: PaginationArgs {
                        limit: 50,
                        cursor: None,
                    },
                },
            )
            .1["items"]
                .as_array()
                .expect("history")
                .len(),
            3
        );

        let applied = run(
            &workspace,
            Command::PruneHistory {
                dry_run: false,
                yes: true,
                vacuum: true,
            },
        )
        .1;
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["revisions_retained"], 2);
        assert_eq!(applied["vacuumed"], true);
        let history = run(
            &workspace,
            Command::History {
                selector: child,
                pagination: PaginationArgs {
                    limit: 50,
                    cursor: None,
                },
            },
        )
        .1;
        assert_eq!(history["items"].as_array().expect("history").len(), 1);
        assert_eq!(history["items"][0]["version"], 3);
    }

    #[test]
    fn northstar_platform_cli_navigation_search_location_and_references_match_specification() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("northstar.mdtree");
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/northstar-platform.snapshot.json");
        assert_eq!(
            run(
                &workspace,
                Command::Import {
                    source: fixture,
                    format: super::SnapshotFormat::Json
                }
            )
            .0,
            EXIT_OK
        );
        let tree = run(&workspace, Command::Tree).1;
        assert_eq!(tree.as_array().expect("tree").len(), 11);
        let search = run(
            &workspace,
            Command::Search {
                query: "domain events kafka".into(),
                scope: "workspace".into(),
                scope_node: None,
                node_types: Vec::new(),
                tags: Vec::new(),
                statuses: Vec::new(),
                min_depth: None,
                max_depth: None,
                created_from: None,
                created_to: None,
                updated_from: None,
                updated_to: None,
                structure: None,
                pagination: PaginationArgs {
                    limit: 10,
                    cursor: None,
                },
            },
        )
        .1;
        assert_eq!(
            search["items"][0]["title"],
            "ADR-002 — Domain Events via Kafka"
        );
        let locate = run(
            &workspace,
            Command::Locate {
                query: "Add architecture decision for API retries".into(),
                node_type: Some("architecture_decision".parse().expect("node type")),
            },
        )
        .1;
        assert_eq!(locate["status"], "recommended");
        assert_eq!(
            locate["candidates"][0]["result"]["title"],
            "Architecture Decisions"
        );
        let references = run(
            &workspace,
            Command::References {
                selector: "payments-service".into(),
                pagination: PaginationArgs {
                    limit: 50,
                    cursor: None,
                },
            },
        )
        .1;
        assert_eq!(references["items"].as_array().expect("references").len(), 2);
        let context = run(
            &workspace,
            Command::Context {
                selector: "architecture-decisions".into(),
                mode: super::ContextMode::Write,
                byte_limit: 8192,
            },
        )
        .1;
        assert_eq!(context["target"]["title"], "Architecture Decisions");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn workspace_navigation_mutation_revision_and_diagnostics_flow_end_to_end() {
        let directory = tempdir().expect("tempdir");
        let workspace = directory.path().join("project.mdtree");
        assert_eq!(
            run(
                &workspace,
                Command::Init {
                    name: "Northstar Platform".into()
                }
            )
            .0,
            EXIT_OK
        );
        let root = run(&workspace, Command::Root).1["id"]
            .as_str()
            .expect("root ID")
            .to_owned();
        let created = run(
            &workspace,
            Command::Create {
                parent: root.clone(),
                title: "Orders".into(),
                content: Some("# Orders\nOrder model".into()),
                dry_run: false,
            },
        )
        .1;
        let child = created["node_id"].as_str().expect("child ID").to_owned();
        assert_eq!(
            run(
                &workspace,
                Command::Children {
                    selector: root.clone(),
                    pagination: PaginationArgs {
                        limit: 50,
                        cursor: None,
                    },
                }
            )
            .1
            .get("items")
            .and_then(serde_json::Value::as_array)
            .expect("children")
            .len(),
            1
        );
        assert_eq!(
            run(
                &workspace,
                Command::Search {
                    query: "order model".into(),
                    scope: "workspace".into(),
                    scope_node: None,
                    node_types: Vec::new(),
                    tags: Vec::new(),
                    statuses: Vec::new(),
                    min_depth: None,
                    max_depth: None,
                    created_from: None,
                    created_to: None,
                    updated_from: None,
                    updated_to: None,
                    structure: None,
                    pagination: PaginationArgs {
                        limit: 10,
                        cursor: None,
                    },
                }
            )
            .1
            .get("items")
            .and_then(serde_json::Value::as_array)
            .expect("matches")[0]["node_id"],
            child
        );
        assert_eq!(
            run(
                &workspace,
                Command::Update {
                    selector: child.clone(),
                    content: "# Orders\nUpdated".into(),
                    expected_version: 1,
                    dry_run: false,
                }
            )
            .1["status"],
            "applied"
        );
        assert_eq!(
            run(
                &workspace,
                Command::ReferenceAdd {
                    source: child.clone(),
                    target: root.clone(),
                    relation: "depends_on".into(),
                    expected_version: 2,
                },
            )
            .1["status"],
            "reference_added"
        );
        assert_eq!(
            run(
                &workspace,
                Command::References {
                    selector: child.clone(),
                    pagination: PaginationArgs {
                        limit: 50,
                        cursor: None,
                    },
                },
            )
            .1
            .get("items")
            .and_then(serde_json::Value::as_array)
            .expect("references")
            .len(),
            1
        );
        let stale = execute(
            &Cli {
                workspace: Some(workspace.clone()),
                workspace_name: None,
                output: OutputFormat::Json,
                no_open: false,
                port: 0,
                command: Some(Command::Update {
                    selector: child.clone(),
                    content: "stale".into(),
                    expected_version: 1,
                    dry_run: false,
                }),
            },
            &mut Vec::new(),
        );
        assert!(stale.is_err());
        assert_eq!(
            run(
                &workspace,
                Command::History {
                    selector: child.clone(),
                    pagination: PaginationArgs {
                        limit: 50,
                        cursor: None,
                    },
                }
            )
            .1["items"]
                .as_array()
                .expect("history items")
                .len(),
            3
        );
        assert_eq!(
            run(
                &workspace,
                Command::Diff {
                    selector: child.clone(),
                    from: 1,
                    to: 2
                }
            )
            .1["changed"],
            true
        );
        let subtree_diff = run(
            &workspace,
            Command::SubtreeDiff {
                from_selector: root.clone(),
                to_selector: child.clone(),
                pagination: PaginationArgs {
                    limit: 1,
                    cursor: None,
                },
            },
        )
        .1;
        assert_eq!(
            subtree_diff["items"]
                .as_array()
                .expect("subtree diff items")
                .len(),
            1
        );
        assert_eq!(subtree_diff["truncated"], true);
        assert_eq!(
            run(
                &workspace,
                Command::RestoreVersion {
                    selector: child.clone(),
                    version: 1,
                    expected_version: 3
                }
            )
            .1["status"],
            "applied"
        );
        assert_eq!(
            run(
                &workspace,
                Command::Remove {
                    selector: child,
                    expected_version: 4,
                    dry_run: true
                }
            )
            .1["deleted"],
            false
        );
        assert_eq!(run(&workspace, Command::Check).0, EXIT_OK);
        assert!(run(&workspace, Command::Unresolved)
            .1
            .as_array()
            .expect("unresolved")
            .is_empty());
    }
}
