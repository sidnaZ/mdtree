//! Deterministic, realistic snapshots used to build the public example databases.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use serde_json::Value;

use crate::{
    hash_content, hash_revision, NodeId, NodeMetadata, NodeType, Reference, ReferenceOrigin,
    ReferenceTarget, ReferenceType, RevisionHashInput, RevisionPolicy, SequentialUlidGenerator,
    Slug, Snapshot, SnapshotNode, SnapshotWorkspace, UlidGenerator, SNAPSHOT_FORMAT_VERSION,
};

const FIXED_TIME: u64 = 1_750_000_000_000;
struct NodeSpec {
    key: &'static str,
    parent: Option<&'static str>,
    title: &'static str,
    kind: &'static str,
    summary: &'static str,
    body: &'static str,
    tags: &'static [&'static str],
}

struct ReferenceSpec {
    source: &'static str,
    target: &'static str,
    kind: &'static str,
}

macro_rules! node {
    ($key:literal, $parent:expr, $title:literal, $kind:literal, $summary:literal, $body:expr) => {
        NodeSpec {
            key: $key,
            parent: $parent,
            title: $title,
            kind: $kind,
            summary: $summary,
            body: $body,
            tags: &[],
        }
    };
    ($key:literal, $parent:expr, $title:literal, $kind:literal, $summary:literal, $body:expr, [$($tag:literal),+ $(,)?]) => {
        NodeSpec {
            key: $key,
            parent: $parent,
            title: $title,
            kind: $kind,
            summary: $summary,
            body: $body,
            tags: &[$($tag),+],
        }
    };
}

/// Returns the approved multi-project developer-work-organizer example.
#[must_use]
pub fn developer_workspace_snapshot() -> Snapshot {
    let specs = vec![
        node!("root", None, "Developer Workspace", "developer_workspace", "Projects, feature knowledge, reusable technical notes, and archived work for one developer.", "This workspace is organized for an AI-assisted software developer. Projects describe stable context, Features explain current work, Archived Features preserve old decisions, and the Knowledge Base keeps reusable technical material."),
        node!("projects", Some("root"), "Projects", "project_collection", "Software projects that may be referenced by features and knowledge.", "Project nodes are stable relation targets. A feature lives in the Features branch and points to every project it affects instead of being duplicated."),
        node!("mdtree", Some("projects"), "MDTree", "project", "Local-first structured Markdown knowledge tree implemented in Rust.", "MDTree provides a portable SQLite knowledge workspace, CLI navigation and maintenance, full-text search, revision history, and an MCP adapter for AI agents."),
        node!("mdtree-summary", Some("mdtree"), "Project Summary", "project_summary", "Purpose, users, and boundaries of MDTree.", "The project helps developers and agents collect durable Markdown knowledge in a navigable tree. It is local-first and single-file; it is not a hosted collaboration service or source-code index."),
        node!("mdtree-env", Some("mdtree"), "Repositories and Environments", "environment", "Repository location and supported development environment.", "The main repository is a Cargo workspace. Development uses the pinned Rust toolchain and bundled SQLite with FTS5. Local examples use `.mdtree` files below the repository or a temporary directory."),
        node!("mdtree-notes", Some("mdtree"), "Project Notes", "project_notes", "Short-lived observations that still have project-level value.", "Keep architectural rules in the shipped project knowledge base. Use this node for cross-feature observations, release reminders, and questions that do not yet justify a dedicated feature."),
        node!("kernel-project", Some("projects"), "Linux Kernel", "project", "Personal study and workstation kernel-configuration work.", "This project tracks reproducible experiments against upstream Linux and the developer workstation configuration. It does not duplicate the kernel source tree."),
        node!("project-b", Some("projects"), "Example Project B", "project", "A small telemetry dashboard used to demonstrate multi-project organization.", "Example Project B collects local service metrics, exposes a small HTTP API, and renders a browser dashboard. It provides a second relation target without using private employer data."),
        node!("project-b-summary", Some("project-b"), "Project Summary", "project_summary", "Scope and user value of the telemetry dashboard.", "The project helps a developer inspect CPU, memory, build, and test trends on a private workstation. It favors understandable local storage over distributed infrastructure."),
        node!("project-b-env", Some("project-b"), "Repositories and Environments", "environment", "Development and runtime environment for Example Project B.", "The API runs locally, the UI is served from the same repository, and sample data is synthetic. Production credentials and external endpoints are outside the example."),
        node!("project-b-notes", Some("project-b"), "Project Notes", "project_notes", "Cross-feature notes for Example Project B.", "Prefer stable JSON response contracts, keep dashboard refreshes bounded, and retain raw samples only long enough to derive daily aggregates."),
        node!("features", Some("root"), "Features", "work_queue", "Current features, including completed work that remains relevant.", "A feature contains enduring intent and design alongside changing plans and progress. Completion is represented by a typed status relation, not by tree position alone."),
        node!("t091", Some("features"), "T091 — 10,000-Node Benchmark Generator", "work_item", "Deterministic large-workspace generator used for performance baselines.", "This feature is complete but remains visible because it is still used for release measurements and regression investigations.", ["benchmark", "done"]),
        node!("t091-intent", Some("t091"), "Intent", "intent", "Why the benchmark generator exists.", "Produce the same representative 10,000-node logical workspace on every run so open, traversal, search, update, rebuild, and context measurements are comparable."),
        node!("t091-design", Some("t091"), "Design", "design", "Determinism and workload-shape decisions.", "Use a fixed seed, stable hierarchy depth, repeatable Markdown sections, metadata, and references. Generate logical state rather than checking a large benchmark database into source control."),
        node!("t091-progress", Some("t091"), "Progress Notes", "progress", "Completion evidence and continuing use.", "The generator and repeatability tests are complete. Criterion benchmarks and resource-measurement examples consume the fixture; future changes must preserve a comparable baseline or document the break."),
        node!("gui", Some("features"), "GUI Workspace Browser", "work_item", "Visual browsing experience for local `.mdtree` databases.", "The feature is in discovery. It should make tree navigation approachable without weakening portability or moving canonical state outside the database.", ["gui", "discovery"]),
        node!("gui-intent", Some("gui"), "Intent", "intent", "User problem and desired outcome.", "Let developers open an unfamiliar workspace, understand its hierarchy, read Markdown, follow relations, and search without learning the complete CLI."),
        node!("gui-behavior", Some("gui"), "Expected Behavior", "behavior", "Observable behavior of the first useful GUI increment.", "Open a local file, display the tree and breadcrumbs, render the selected node, show backlinks and outgoing relations, search content, and report invalid or incompatible workspaces clearly."),
        node!("gui-design", Some("gui"), "Design", "design", "Initial technical boundaries.", "Start read-only and call shared application services rather than querying tables independently. Keep database access local, surface bounded results, and do not introduce a second canonical cache."),
        node!("gui-plan", Some("gui"), "Implementation Plan", "plan", "Proposed increments for delivering the browser.", "First evaluate a lightweight cross-platform UI toolkit. Then implement file opening and diagnostics, navigation and Markdown rendering, search and references, packaging, and usability tests against the example workspaces."),
        node!("gui-acceptance", Some("gui"), "Acceptance Criteria", "acceptance_criteria", "Conditions for considering the first increment useful.", "A user can open both public examples, navigate every node, search for benchmark and build guidance, follow typed relations, and receive actionable errors for missing, corrupt, and unsupported files."),
        node!("gui-questions", Some("gui"), "Open Questions", "open_questions", "Decisions intentionally left unresolved during discovery.", "Choose the UI toolkit, decide whether diagrams belong in the first release, determine packaging targets, and measure how large a tree can remain fully expanded without harming responsiveness."),
        node!("gui-progress", Some("gui"), "Progress Notes", "progress", "Current state of the GUI investigation.", "The user workflows and read-only scope are recorded. Toolkit evaluation, wireframes, and an implementation task breakdown have not started."),
        node!("archived", Some("root"), "Archived Features", "work_queue", "Completed feature knowledge removed from the active working set.", "Archival changes parentage, not identity. History and typed project or status relations remain available, and a feature may move back if work resumes."),
        node!("arch001", Some("archived"), "ARCH-001 — Initial Architecture Planning", "work_item", "Early planning that established the crate boundaries and canonical-storage model.", "This planning is complete and no longer active, but it remains useful for understanding why the implementation separates domain, Markdown, SQLite, CLI, and MCP concerns."),
        node!("arch-intent", Some("arch001"), "Intent", "intent", "Problem the initial architecture needed to solve.", "Design a portable local knowledge system that serves humans and agents without duplicating business logic across interfaces."),
        node!("arch-req", Some("arch001"), "Requirements", "requirements", "Constraints that shaped the architecture.", "Use one canonical local file, preserve stable identities and history, support deterministic Markdown-derived search, provide bounded agent context, and keep mutations transactional."),
        node!("arch-alts", Some("arch001"), "Alternatives Considered", "alternatives", "Major designs considered before implementation.", "A directory of Markdown files was simpler to inspect but weak for atomic updates and derived indexes. A hosted service improved collaboration but violated the local-first scope. Adapter-specific logic was rejected because CLI and MCP behavior would drift."),
        node!("arch-final", Some("arch001"), "Final Architecture", "architecture", "The selected crate and persistence boundaries.", "Core owns the domain contracts, Markdown owns pure parsing, SQLite owns transactions and queries, and CLI and MCP remain thin adapters. The `.mdtree` SQLite database is canonical."),
        node!("arch-decision", Some("arch001"), "Decision Summary", "decision", "Concise outcome of the archived planning work.", "Adopt a ports-and-adapters Cargo workspace with canonical SQLite persistence, deterministic derived records, and separate human and agent interfaces."),
        node!("statuses", Some("root"), "Task Statuses", "status_collection", "Canonical relation targets for feature lifecycle status.", "Feature status is expressed with `has_status` relations so agents can query it consistently without duplicating status strings in metadata."),
        node!("done", Some("statuses"), "Done", "task_status", "Work that meets its acceptance criteria and requires no implementation activity.", "Done features may remain under Features while still operationally relevant or move to Archived Features when they become historical."),
        node!("kb", Some("root"), "Knowledge Base", "knowledge_base", "Reusable technical knowledge not owned by one feature.", "This branch holds durable notes, examples, and personal development practices. Feature-specific decisions remain with their feature and may reference these topics."),
        node!("llms", Some("kb"), "LLMs", "topic", "Reusable practices for prompting and running language models.", "Keep examples provider-neutral where possible and record local commands separately from reusable prompt structures."),
        node!("prompts", Some("llms"), "Reusable Prompts", "topic", "Prompt collections that can be adapted across repositories and agents.", "Each child contains multiple variants rather than one brittle prompt. Replace placeholders with observed repository context before use."),
        node!("orientation-prompts", Some("prompts"), "Codebase Orientation Prompts", "topic", "Prompts for mapping an unfamiliar repository before changing it.", "Variants ask an agent to identify entry points, architectural boundaries, data ownership, build and test commands, and the smallest files relevant to a requested change. Every answer should distinguish evidence from inference."),
        node!("planning-prompts", Some("prompts"), "Implementation Planning Prompts", "topic", "Prompts for turning an outcome into a verifiable change plan.", "Variants request affected contracts, ordered implementation steps, migration or compatibility risks, tests by risk, and explicit decisions that require human approval."),
        node!("review-prompts", Some("prompts"), "Change Review Prompts", "topic", "Prompts for reviewing correctness, regressions, and maintainability.", "Variants focus separately on observable bugs, concurrency and persistence safety, public-contract compatibility, missing tests, and unnecessary complexity. Findings cite concrete files or behaviors."),
        node!("local-llms", Some("llms"), "Running LLMs Locally", "topic", "Practical notes for private local inference workflows.", "Treat model files as replaceable dependencies, measure the complete workflow rather than headline token speed, and keep sensitive prompts on trusted machines."),
        node!("llm-commands", Some("local-llms"), "Example Commands", "command_collection", "Provider-neutral command patterns for serving and testing local models.", "Record commands for starting an OpenAI-compatible local endpoint, listing models, sending a small deterministic request, checking context limits, and capturing latency and memory. Pin actual tool versions in machine-specific notes."),
        node!("model-choice", Some("local-llms"), "Choosing Models by Task", "guideline", "Match model capability and resource cost to the development task.", "Use small models for classification, extraction, and repetitive edits; stronger coding models for multi-file changes; and long-context models only when retrieval cannot narrow the evidence. Validate on representative prompts."),
        node!("local-api", Some("local-llms"), "Local Inference APIs", "topic", "Common API concepts for integrating local model servers.", "Document endpoint base URL, model identifier, chat or response schema, streaming behavior, context and output limits, authentication expectations, and timeout policy. Do not assume every OpenAI-compatible server implements every extension."),
        node!("kernel-kb", Some("kb"), "Linux Kernel", "topic", "Knowledge about the developer workstation kernel.", "Keep reproducible customization intent and validation here; keep upstream source exploration under the Linux Kernel project."),
        node!("kernel-custom", Some("kernel-kb"), "Developer Workstation Customizations", "topic", "Intent, validation, and rollback notes for every non-default kernel choice.", "Each customization explains why it exists, the relevant configuration symbols, how to verify behavior, and how to return to the distribution kernel."),
        node!("bpf", Some("kernel-custom"), "Enable BPF Development Support", "configuration", "Kernel capabilities needed for local tracing and BPF experiments.", "Enable BPF syscall support, BTF debug information, common tracing events, and the required networking options. Validate with the local tracing toolchain and retain the previous boot entry for rollback."),
        node!("drivers", Some("kernel-custom"), "Disable Unused Hardware Drivers", "configuration", "Reduce build time by excluding hardware families absent from the workstation.", "Start from the distribution configuration, inspect detected hardware before removing drivers, retain storage, input, networking, and recovery dependencies, and boot-test the result before deleting the known-good kernel."),
        node!("vscode", Some("kb"), "VS Code", "topic", "Reproducible editor practices for the developer workspace.", "Store project-owned settings in repositories and keep personal preferences separate. Avoid requiring an extension when a standard build command is sufficient."),
        node!("vscode-settings", Some("vscode"), "Workspace Settings", "configuration", "Shared editor settings that improve consistency without overriding personal ergonomics.", "Record formatter selection, language-server behavior, file exclusions, test discovery, and save actions. Explain unusual settings so contributors can evaluate them."),
        node!("vscode-ext", Some("vscode"), "Recommended Extensions", "tooling", "Small, justified extension sets for supported languages and workflows.", "Recommend extensions by capability, document why each is useful, and avoid workspace recommendations that send source code to external services without an explicit trust decision."),
        node!("vscode-debug", Some("vscode"), "Debugging Configurations", "configuration", "Repeatable launch and attach configurations.", "Prefer configurations that build the selected target, pass workspace paths explicitly, preserve stderr diagnostics, and avoid embedding credentials."),
        node!("vscode-tasks", Some("vscode"), "Build and Task Integrations", "tooling", "Editor shortcuts that delegate to canonical repository commands.", "Tasks should call Cargo, Make, or documented scripts instead of reimplementing build logic. Keep CI and terminal commands usable without VS Code."),
    ];
    let references = vec![
        ReferenceSpec {
            source: "t091",
            target: "mdtree",
            kind: "applies_to",
        },
        ReferenceSpec {
            source: "t091",
            target: "done",
            kind: "has_status",
        },
        ReferenceSpec {
            source: "gui",
            target: "mdtree",
            kind: "applies_to",
        },
        ReferenceSpec {
            source: "arch001",
            target: "mdtree",
            kind: "applies_to",
        },
        ReferenceSpec {
            source: "arch001",
            target: "done",
            kind: "has_status",
        },
        ReferenceSpec {
            source: "orientation-prompts",
            target: "mdtree",
            kind: "informs",
        },
        ReferenceSpec {
            source: "kernel-custom",
            target: "kernel-project",
            kind: "informs",
        },
        ReferenceSpec {
            source: "vscode-tasks",
            target: "mdtree",
            kind: "informs",
        },
    ];
    build_snapshot("Developer Workspace", 20_000, specs, references)
}

fn build_snapshot(
    name: &str,
    id_start: u64,
    specs: Vec<NodeSpec>,
    reference_specs: Vec<ReferenceSpec>,
) -> Snapshot {
    let generator = SequentialUlidGenerator::new(id_start);
    let mut ids = HashMap::new();
    for spec in &specs {
        ids.insert(spec.key, NodeId::new(generator.generate()));
    }
    let mut sibling_orders = HashMap::<Option<&str>, u32>::new();
    let mut nodes = Vec::with_capacity(specs.len());
    for spec in specs {
        let id = ids[spec.key];
        let parent_id = spec.parent.map(|parent| ids[parent]);
        let sibling_order = sibling_orders.entry(spec.parent).or_default();
        let order = *sibling_order;
        *sibling_order += 1;
        let mut metadata = NodeMetadata::new(spec.title);
        metadata.summary = Some(spec.summary.into());
        metadata.node_type = Some(NodeType::from_str(spec.kind).expect("fixture node type"));
        metadata.tags = spec.tags.iter().map(|tag| (*tag).into()).collect();
        metadata
            .extensions
            .insert("owner".into(), Value::String("example-developer".into()));
        let markdown = if spec.body.starts_with("# ") {
            spec.body.to_owned()
        } else {
            format!("# {}\n\n{}\n", spec.title, spec.body)
        };
        let slug = crate::generate_slug(spec.title, std::iter::empty::<&Slug>());
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: id,
            parent_id,
            slug: &slug,
            metadata: &metadata,
            markdown_content: &markdown,
            sibling_order: order,
        })
        .expect("fixture revision hash");
        nodes.push(SnapshotNode {
            id,
            parent_id,
            slug,
            metadata,
            markdown_content: markdown.clone(),
            sibling_order: order,
            version: 1,
            content_hash: hash_content(&markdown),
            revision_hash,
            created_at: FIXED_TIME,
            updated_at: FIXED_TIME,
        });
    }
    let references = reference_specs
        .into_iter()
        .map(|spec| Reference {
            source_node_id: ids[spec.source],
            source_section_id: None,
            reference_type: ReferenceType::from_str(spec.kind).expect("fixture reference type"),
            target: ReferenceTarget::Resolved {
                node_id: ids[spec.target],
                target_ref: Some(spec.target.into()),
                anchor: None,
            },
            origin: ReferenceOrigin::Explicit,
            metadata: BTreeMap::new(),
        })
        .collect();
    Snapshot {
        format: "mdtree-snapshot".into(),
        format_version: SNAPSHOT_FORMAT_VERSION,
        workspace: SnapshotWorkspace {
            name: name.into(),
            workspace_format_version: 1,
        },
        revision_policy: RevisionPolicy::HeadOnly,
        nodes,
        revisions: Vec::new(),
        references,
    }
}

#[cfg(test)]
mod tests {
    use super::developer_workspace_snapshot;
    use crate::validate_snapshot;

    #[test]
    fn developer_workspace_snapshot_is_deterministic_and_valid() {
        let developer = developer_workspace_snapshot();
        assert!(validate_snapshot(&developer).is_valid());
        assert_eq!(developer, developer_workspace_snapshot());
        assert_eq!(developer.nodes.len(), 52);
        assert_eq!(developer.references.len(), 8);
    }
}
