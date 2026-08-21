# MDTree Developer Knowledge Base

This guide is the starting point for engineers who are new to MDTree. It
explains how the project fits together, how to run it, where changes belong,
and which checks to run before submitting work.

Commands in this document assume the repository root is the current directory.
When behavior and documentation disagree, treat the source code and current
tests as authoritative, then update this guide.

## 1. Project in five minutes

MDTree is a local-first knowledge base for hierarchical Markdown. A workspace
is a portable SQLite database, conventionally named `.mdtree`, containing:

- one canonical root and an ordered tree of nodes;
- Markdown content split into derived searchable sections;
- typed outgoing references and backlinks;
- immutable node history;
- lexical search data;
- optional derived semantic embeddings.

The same workspace is exposed through three delivery adapters:

- `mdtree`, the complete command-line interface;
- `mdtree-mcp`, a stdio Model Context Protocol server for AI agents;
- `mdtree browse-ui`, the interactive web viewer and editor.

Lexical search is always available and remains the default. Semantic search is
optional and uses a local Ollama embedding provider. Hybrid search combines
lexical and semantic results.

Current source-of-truth compatibility values are:

- package version: `0.2.0`;
- minimum Rust version: `1.88`;
- workspace format: `1`;
- SQLite schema version: `7`.

The package version comes from the root `Cargo.toml`. Format and schema versions
come from `crates/mdtree-sqlite/src/migrations.rs`. Do not infer current
compatibility from old release notes.

## 2. First-day setup

### Prerequisites

Required:

- Rust 1.88 or newer;
- Cargo;
- a C toolchain suitable for building Rust dependencies;
- Git.

The checked-in `rust-toolchain.toml` selects stable Rust with `rustfmt` and
Clippy. SQLite is bundled through `rusqlite`, so a system SQLite development
package is normally unnecessary.

Optional:

- Ollama, only for semantic and hybrid search;
- Codex or Claude Code, only for exercising the MCP integration;
- `curl`, used by the documentation smoke test for the web UI.

### Build and test

```bash
cargo build --workspace --locked
cargo test --workspace --locked
```

For an optimized build:

```bash
cargo build --release --locked
```

### Install the user-facing binaries

Install only the CLI and MCP server:

```bash
./build-and-install.sh
```

The default destination is `$HOME/.local/bin`. Supply another directory as the
first argument if needed:

```bash
./build-and-install.sh /some/bin/directory
```

Alternatively, use Cargo:

```bash
cargo install --locked --path crates/mdtree-cli
cargo install --locked --path crates/mdtree-mcp
```

### Prove the local build works

Use a disposable workspace:

```bash
workdir=$(mktemp -d)
cargo run -p mdtree-cli -- \
  --workspace "$workdir/onboarding.mdtree" init "Onboarding"
cargo run -p mdtree-cli -- \
  --workspace "$workdir/onboarding.mdtree" create onboarding "First note"
cargo run -p mdtree-cli -- \
  --workspace "$workdir/onboarding.mdtree" tree
cargo run -p mdtree-cli -- \
  --workspace "$workdir/onboarding.mdtree" check
```

Remove the disposable directory when finished.

For a broader checked-in smoke test:

```bash
bash scripts/smoke-docs.sh
```

## 3. Architecture

MDTree follows a domain-and-adapters structure. Dependency arrows point toward
the code being reused:

```text
                         +------------------+
                         |   mdtree-core    |
                         | domain + ports   |
                         +------------------+
                           ^       ^      ^
                           |       |      |
              +------------+       |      +-------------+
              |                    |                    |
   +-------------------+  +-------------------+  +-------------------+
   | mdtree-markdown   |  |  mdtree-sqlite   |  | mdtree-semantic   |
   | parse + snapshots |<-| persistence      |<-| Ollama + ranking  |
   +-------------------+  +-------------------+  +-------------------+
              ^                    ^                    ^
              +--------------------+--------------------+
                                   |
              +--------------------+--------------------+
              |                    |                    |
      +---------------+    +---------------+    +---------------+
      |  mdtree-cli   |    |  mdtree-mcp   |    |  mdtree-web   |
      | terminal      |    | AI adapter    |    | browser/API   |
      +---------------+    +---------------+    +---------------+
```

The diagram is conceptual. In Cargo dependencies, SQLite uses both core and
Markdown, semantic uses core, Markdown, and SQLite, and the delivery adapters
compose the services they need.

The main design rule is more important than the precise arrows:

> Tree, identity, selector, version, and mutation rules belong in shared domain
> or persistence services. CLI, MCP, and web code translate input and output;
> they must not implement competing versions of those rules.

The web crate documents this explicitly and routes structural mutations through
the same `prepare_node_mutation` and `SqliteStore` path used by the other
adapters.

## 4. Crate map

| Crate | Responsibility | Start reading |
| --- | --- | --- |
| `mdtree-core` | Domain types, ports, navigation contracts, search requests, errors, snapshots, hashes, pagination | `crates/mdtree-core/src/lib.rs`, `ports.rs`, `node.rs`, `identity.rs` |
| `mdtree-markdown` | Frontmatter, headings, anchors, Markdown links, derived records, JSON/Markdown snapshots | `crates/mdtree-markdown/src/lib.rs` |
| `mdtree-sqlite` | Connections, migrations, workspace lifecycle, transactions, projections, FTS, history, backup, integrity, semantic storage | `crates/mdtree-sqlite/src/lib.rs`, `store.rs`, `workspace.rs` |
| `mdtree-semantic` | Ollama provider, chunk indexing, exact-vector search, hybrid ranking | `crates/mdtree-semantic/src/lib.rs`, `indexer.rs`, `search.rs`, `hybrid.rs` |
| `mdtree-cli` | Clap command surface, text/JSON/JSONL output, interactive terminal browser, web launcher | `crates/mdtree-cli/src/lib.rs` |
| `mdtree-mcp` | MCP resources and tools, read/write mode, semantic calls, workspace switching | `crates/mdtree-mcp/src/lib.rs`, `mutation.rs`, `switching.rs` |
| `mdtree-web` | Axum server, embedded static assets, reads, WebSockets, structural editor, lifecycle and security | `crates/mdtree-web/src/lib.rs`, `api.rs`, `commands.rs`, `security.rs` |

Useful dependency-boundary checks:

- Core has no dependency on SQLite, HTTP, MCP, terminal, or web code.
- Markdown parsing does not write a workspace by itself.
- Canonical persistence and optimistic transactions stay in SQLite services.
- Semantic data is derived; canonical writes must not require Ollama.
- Every adapter should expose stable domain errors instead of inventing
  incompatible rules.

## 5. Core concepts and invariants

### Workspace

A workspace is one SQLite file. New workspaces are migrated to the latest
supported schema when opened. A workspace has exactly one root node.

The default path for both binaries is `.mdtree` in the current directory. Set a
different path with:

```bash
mdtree --workspace path/to/project.mdtree status
```

or:

```bash
export MDTREE_WORKSPACE=path/to/project.mdtree
```

Do not assume the repository-root `.mdtree` is project documentation. It is a
development workspace and its content can be unrelated to MDTree itself.

### Node identity and selectors

Canonical nodes have stable ULID identities. User-facing operations accept a
`NodeSelector`, which can normally be:

- a stable node ID;
- a slug;
- a canonical root-to-node path.

Paths and slugs are navigation conveniences; IDs are the durable identity.
Renaming may change a slug and canonical path without changing the node ID.
When ambiguity matters, prefer the ID or full canonical path.

### Canonical and derived data

Canonical data includes current nodes, metadata, tree placement, and explicit
references. Derived data includes parsed sections, FTS rows, inferred Markdown
references, and semantic chunks or embeddings.

A canonical mutation must update its required derived records transactionally.
If derived records are suspected to be stale, use:

```bash
mdtree rebuild-indexes
```

Do not manually edit derived tables.

### Ordering

Children have a zero-based `sibling_order`. Reads use deterministic tie-breaking
so results remain stable. Reorder and move operations must preserve a valid,
contiguous canonical order and must go through the shared mutation path.

### Versions and optimistic concurrency

Every current node has a content version. Update, rename, move, reorder,
reference, restore, and remove operations require the version observed by the
caller. This prevents a stale client from silently overwriting newer work.

Typical flow:

1. Read the node with `show` or assemble write context.
2. Record its current version.
3. Submit the mutation with `--expected-version`.
4. If a version conflict occurs, reread, reconcile, and retry.

Never bypass this check in an adapter.

### History

Each accepted mutation creates immutable history. Restoring an old revision
creates a new head revision; it does not rewrite history.

`prune-history` is different: it permanently deletes retained historical
revisions. Always use `--dry-run` first and take a backup before confirming it.

### References

References are typed directed edges. Explicit references are canonical user
intent. Markdown-derived references come from parsed content. Backlinks are
incoming edges. Targets can be resolved or unresolved.

Reference types are workspace data, not a hard-coded global enum. Preserve that
property when adding UI or API behavior.

### Pagination

Bounded collection operations use a limit from 1 through 100 and return an
opaque continuation cursor. A cursor belongs to the operation and state that
created it. Do not parse it, edit it, or synthesize one in clients.

## 6. Everyday workspace operations

The CLI's global output modes are `text`, `json`, and `jsonl`:

```bash
mdtree --output json status
```

Use JSON or JSONL in scripts rather than parsing human-readable text.

### Create and inspect

```bash
mdtree --workspace team.mdtree init "Team knowledge"
mdtree --workspace team.mdtree root
mdtree --workspace team.mdtree tree
mdtree --workspace team.mdtree create team-knowledge "Architecture"
mdtree --workspace team.mdtree show architecture
mdtree --workspace team.mdtree path architecture
```

`create` defaults the body to a title heading. Supply complete Markdown with
`--content` when needed.

### Navigate large trees

Use bounded operations instead of loading an entire workspace:

```bash
mdtree children architecture --limit 50
mdtree descendants architecture --order dfs --limit 50
mdtree subtree architecture --order bfs --limit 50
mdtree ancestors payment-service
mdtree siblings payment-service
mdtree statistics architecture
```

Use the returned cursor with the same command's `--cursor` option to continue.

### Search and locate

```bash
mdtree search "SQLite migration"
mdtree search "embedding provider" --scope subtree --scope-node architecture
mdtree locate "where should backup documentation live?"
mdtree examples architecture-decisions
mdtree inspect architecture --depth 2
```

`locate` recommends a structural destination. It does not mutate the tree.

### Safe mutations

Read the current version before changing an existing node:

```bash
mdtree --output json show architecture
mdtree update architecture \
  --content '# Architecture' \
  --expected-version 1 \
  --dry-run
mdtree update architecture \
  --content '# Architecture' \
  --expected-version 1
```

The same pattern applies to rename, move, clone, remove, reference changes, and
version restoration. Prefer a dry run when the command supports it.

For several related changes, prefer `mutation-batch` so validation and commit
are all-or-nothing. `atomic-tree-batch` is the narrower batch for unrelated
moves and removals. Both accept a JSON file or `-` for standard input.

### History and comparison

```bash
mdtree history architecture
mdtree revision architecture 1
mdtree diff architecture 1 2
mdtree subtree-diff old-architecture new-architecture
mdtree restore-version architecture 1 --expected-version 2
```

### Integrity and recovery

```bash
mdtree status
mdtree validate
mdtree check
mdtree doctor
mdtree backup backup.mdtree
mdtree restore backup.mdtree --overwrite
```

- `status` reports versions and counts.
- `validate` returns bounded, resumable MDTree invariant findings.
- `check` validates SQLite and MDTree invariants.
- `doctor` diagnoses runtime and workspace health without repairing it.
- `backup` uses an online-safe SQLite backup operation.
- `restore` validates the source and requires `--overwrite` to replace an
  existing destination.

Use the commands above before reaching for a SQLite shell.

### Import and export

Whole-workspace snapshots:

```bash
mdtree export snapshot.json --format json
mdtree export snapshot-directory --format markdown
mdtree --workspace imported.mdtree import snapshot.json --format json
```

Import creates a new workspace. Export a selected node or subtree as Markdown:

```bash
mdtree export-node architecture exported-architecture --subtree
mdtree export-node architecture exported-architecture --subtree --depth 2
```

The files in `examples/` are useful fixtures. See `examples/README.md` for
commands and regeneration rules.

## 7. Running the interfaces

### CLI

During development, avoid reinstalling after every change:

```bash
cargo run -p mdtree-cli -- --workspace example.mdtree status
```

Arguments before the second `--` belong to Cargo; arguments after it belong to
MDTree.

Generate shell completions with:

```bash
mdtree completions bash
mdtree completions zsh
mdtree completions fish
```

### MCP server

The MCP server communicates over standard input/output. Logs go to standard
error so they do not corrupt MCP framing.

It is read-only by default:

```bash
mdtree-mcp path/to/workspace.mdtree
```

Enable writes explicitly:

```bash
mdtree-mcp --allow-write path/to/workspace.mdtree
```

`MDTREE_MCP_ALLOW_WRITE` also enables writes when its value is `1`, `true`, or
`yes`.

Workspace switching is a separate capability. It must be enabled explicitly,
and every allowed root should be narrow:

```bash
mdtree-mcp \
  --allow-write \
  --allow-workspace-switch \
  --workspace-root /absolute/path/to/approved/root \
  /absolute/path/to/initial.mdtree
```

`--workspace-root` is rejected unless `--allow-workspace-switch` is present.
Do not authorize a broad filesystem root merely for convenience.

If no explicit workspace or `MDTREE_WORKSPACE` is supplied, the server prefers
an existing `.mdtree`. A fallback may be supplied with
`--fallback-workspace` or `MDTREE_FALLBACK_WORKSPACE`.

Example registrations from the repository root:

```bash
codex mcp add mdtree -- \
  mdtree-mcp --allow-write --allow-workspace-switch --workspace-root .

claude mcp add --transport stdio --scope user mdtree -- \
  mdtree-mcp --allow-write --allow-workspace-switch --workspace-root .
```

When developing MCP behavior, test both modes. A write tool must not merely fail
in read-only mode; it should not be exposed there.

### Web UI

Start it in the background and open the browser:

```bash
mdtree --workspace project.mdtree browse-ui
```

Useful options:

```bash
mdtree --workspace project.mdtree --no-open browse-ui
mdtree --workspace project.mdtree --port 4000 browse-ui --foreground
mdtree --workspace project.mdtree browse-ui \
  --also-workspace other.mdtree="Other project"
```

The server prints its URL. It uses a per-launch random session credential for
mutations, WebSocket setup, and shutdown, and rejects mismatched browser
origins.

Important security boundary: the listener binds to all IPv4 interfaces. The web
UI is not an authenticated multi-user collaboration service. Use it only on a
trusted network or behind operating-system/network controls appropriate to the
environment. Do not expose it directly to the public Internet.

## 8. Search and semantic indexing

### Lexical search

Lexical search uses SQLite FTS5 over derived section records. It works without
Ollama and is the compatibility default:

```bash
mdtree search "transaction rollback" --mode lexical
```

Search can be scoped and filtered by node type, tags, reference status, depth,
timestamps, and leaf/internal structure. Keep filtering in shared search
contracts so CLI, MCP, and web results agree.

### Ollama configuration

Semantic operations use:

- `MDTREE_OLLAMA_URL`, defaulting to the local provider URL in
  `mdtree-semantic`;
- `MDTREE_OLLAMA_MODEL`;
- `MDTREE_OLLAMA_TIMEOUT_SECONDS`, default `60`.

Equivalent CLI and MCP flags exist for URL, model, and timeout. Provider
credentials must not be embedded in the workspace. The provider validates
model responses, dimensions, count, and finite vector values.

### Index lifecycle

Select an embedding model, then build:

```bash
export MDTREE_OLLAMA_MODEL=your-embedding-model
mdtree semantic-index build
mdtree semantic-index status
```

Operational commands:

```bash
mdtree semantic-index resume
mdtree semantic-index retry
mdtree semantic-index clear --dry-run
mdtree semantic-index clear --yes
```

The semantic index is derived data. Canonical writes and lexical search do not
call Ollama. A model/profile mismatch must be handled explicitly; do not mix
vectors from incompatible profiles.

Search examples:

```bash
mdtree search "payment retry strategy" --mode semantic
mdtree search "payment retry strategy" --mode hybrid
mdtree search "payment retry strategy" \
  --mode hybrid --hybrid-fallback
```

Hybrid fallback is opt-in and should remain visibly labelled. Without it, a
semantic provider failure is an error instead of silently changing retrieval
behavior.

### Performance acceptance

The initial implementation performs an exact vector scan. Reproduce its
checked-in scale benchmark with:

```bash
cargo bench -p mdtree-sqlite --bench services -- \
  semantic_exact_scan_3000x384 \
  --sample-size 10 \
  --warm-up-time 0.1 \
  --measurement-time 0.5
```

Read `crates/mdtree-semantic/ACCEPTANCE.md` before changing the algorithm. Any
ANN or extension-based replacement must preserve filters, stable ties,
explanations, profile compatibility, cursors, and the exact-scan fallback.

## 9. How to implement common changes

### Add or change a domain concept

1. Define the invariant and stable error in `mdtree-core`.
2. Add or extend a port in `crates/mdtree-core/src/ports.rs` if persistence is
   needed.
3. Implement the port and transaction in `mdtree-sqlite`.
4. Add core tests for pure rules and SQLite tests for persistence behavior.
5. Expose the operation through only the adapters that need it.
6. Verify adapter outputs preserve stable error categories.

Do not begin by adding separate logic to CLI, MCP, and web.

### Add a CLI command

1. Add a Clap variant in `crates/mdtree-cli/src/lib.rs`.
2. Reuse a core/SQLite/semantic service in `execute`.
3. Support the shared text, JSON, and JSONL conventions where applicable.
4. Use stable exit codes: `0` success, `2` invalid input/findings, `1`
   operational failure.
5. Add parser and execution tests.
6. Update `scripts/smoke-docs.sh` if the command is part of the documented
   essential workflow.

### Add an MCP tool

1. Confirm a shared service already implements the operation.
2. Add parameter and result types in `mdtree-mcp`.
3. Keep pagination bounded and cursors opaque.
4. Decide deliberately whether the tool is read-only or mutating.
5. For mutations, require optimistic preconditions and use an operation ID
   where retry idempotency is part of the contract.
6. Add the tool only to the appropriate access-mode router.
7. Test in-process behavior and the stdio protocol in
   `crates/mdtree-mcp/tests/stdio.rs`.
8. Update the server-provided tool instructions when callers need a sequencing
   or safety rule.

### Add a web operation

1. Put reads in the existing API/search services.
2. Put structural mutation messages through `commands.rs`.
3. Use the authoritative workspace store; do not mutate browser-side state as
   if it were canonical.
4. Require the version observed by the client.
5. Authenticate mutations through the existing session/WebSocket mechanism.
6. Preserve same-origin checks and sanitized Markdown rendering.
7. Broadcast accepted changes so other connected clients refresh.
8. Test lifecycle behavior, rejected stale writes, and security boundaries.

Static assets are embedded from `crates/mdtree-web/assets/`. `package.json` is
used for asset tooling; the shipped server is still a Rust binary with embedded
assets.

### Add a SQLite migration

1. Add the next zero-padded SQL file under
   `crates/mdtree-sqlite/migrations/`.
2. Append it to `MIGRATIONS` in `migrations.rs`.
3. Increment `LATEST_SCHEMA_VERSION`.
4. Make the migration deterministic and compatible with every supported older
   schema.
5. Add tests for fresh creation and upgrade from the preceding schema.
6. Test that a failure rolls back the complete migration batch.
7. Test concurrent openers when the migration changes startup behavior.
8. Update this guide, status expectations, release notes, and fixtures.

Never edit a released migration in place. A database newer than the executable
must be rejected rather than guessed at.

### Change Markdown parsing

Work in `mdtree-markdown`, then verify:

- heading and anchor stability;
- byte ranges;
- frontmatter round trips;
- Markdown and wiki-link extraction;
- derived section and FTS records;
- JSON and Markdown snapshot round trips;
- rebuild behavior for existing workspaces.

A parsing change can affect search, references, hashes, history, and snapshot
compatibility, so test all of those boundaries when relevant.

### Change semantic search

Keep provider I/O in `mdtree-semantic` and storage in `mdtree-sqlite`.
Canonical mutation paths must remain provider-independent. Update deterministic
relevance fixtures and `ACCEPTANCE.md` when ranking behavior or performance
evidence changes.

## 10. Testing and quality gates

### Fast local loop

Run the narrowest relevant test first:

```bash
cargo test -p mdtree-core
cargo test -p mdtree-sqlite
cargo test -p mdtree-mcp
cargo test -p mdtree-cli
cargo test -p mdtree-web
```

Filter to one test while iterating:

```bash
cargo test -p mdtree-sqlite migration_test_name
```

### Required repository checks

Before submitting a cross-cutting change:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
bash scripts/smoke-docs.sh
```

For packaging or release changes, also run the release build and package
verification described below.

### Where tests belong

- Pure domain rule: unit test in `mdtree-core`.
- Markdown parsing or snapshot encoding: `mdtree-markdown`.
- Transaction, migration, query, backup, or derived-record behavior:
  `mdtree-sqlite`.
- Provider, indexing, or rank fusion: `mdtree-semantic`.
- CLI parsing/output/exit code: `mdtree-cli`.
- MCP exposure, schema, access mode, or stdio framing: `mdtree-mcp`.
- HTTP, WebSocket, rendering, lifecycle, or browser security: `mdtree-web`.
- Complete user journey: adapter integration test or `scripts/smoke-docs.sh`.

Tests must not require a live Ollama provider unless they are explicitly
labelled manual/acceptance tests. Normal workspace and lexical suites must stay
offline and deterministic.

### Fixtures

Prefer `tempfile` for workspaces created by tests. Checked-in examples have
specific regeneration rules in `examples/README.md`. Do not overwrite the
repository-root `.mdtree` while regenerating examples.

## 11. Database and compatibility rules

- A workspace is a portable SQLite file, not a directory of authoritative
  Markdown files.
- Only one writer should mutate a workspace at a time.
- Migrations are embedded, ordered, and applied in one immediate transaction.
- Foreign keys are enabled when connections are opened.
- The workspace format version and SQLite schema version are different
  compatibility dimensions.
- A newer unsupported schema is an error.
- Canonical mutations and their required derived updates are transactional.
- Mutation receipts support idempotent retries where an operation ID is used.
- Backups should use the provided online-safe operation, not a raw file copy of
  a live database.
- Destructive maintenance requires an explicit confirmation path and should
  have a dry run.

When changing serialized JSON, MCP results, cursor encoding, error codes, or
snapshot formats, treat the change as a compatibility decision rather than a
local refactor.

## 12. Debugging playbook

### The workspace does not open

Run:

```bash
mdtree --workspace suspect.mdtree doctor
mdtree --workspace suspect.mdtree status
mdtree --workspace suspect.mdtree check
```

Check that the path is the intended workspace, the process can read/write it,
and its schema is not newer than the executable.

### A selector resolves incorrectly or is ambiguous

Inspect the canonical tree and path:

```bash
mdtree tree
mdtree path candidate-slug
```

Retry with the stable node ID or full canonical path.

### A mutation reports a version conflict

Another writer changed the node after it was read. Fetch the current node,
reconcile the intended change, and submit the new current version. Do not
blindly increment `expected_version`.

### Search misses recently changed content

First confirm the canonical node:

```bash
mdtree show node-selector
mdtree check
```

If derived records are stale or damaged:

```bash
mdtree rebuild-indexes
mdtree check
```

For semantic-only misses, also inspect:

```bash
mdtree semantic-index status
```

Resume pending work, retry failed chunks, or rebuild with a compatible profile.

### MCP does not connect

- Run the exact configured command in a terminal and inspect standard error.
- Confirm `mdtree-mcp` is on `PATH`.
- Confirm the workspace path and authorized roots.
- Remember the workspace path is positional; `--workspace-root` authorizes
  switching and does not select the initial workspace.
- Ensure no normal log or debug output is written to standard output.
- Confirm write tools are expected only when `--allow-write` is enabled.

### Web UI does not start

- Run with `browse-ui --foreground` to keep errors attached.
- Use `--port 0` or omit the port to let the OS select one.
- Confirm every `--also-workspace` path opens successfully.
- If the browser is unavailable, use `--no-open` and open the printed URL.
- Check local firewall/network policy because the listener uses all IPv4
  interfaces.

### Ollama operations fail

- Confirm the URL, model, and timeout configuration.
- Confirm the selected model produces embeddings.
- Check semantic-index status for failed chunks.
- Do not clear the index until a dry run shows the expected scope.
- Never include document content in provider error messages or logs.

The binaries initialize structured tracing to standard error. No dedicated
`RUST_LOG` filter is currently wired into the default subscriber, so add
targeted temporary diagnostics carefully and remove or formalize them before
merging.

## 13. Release process

Before packaging:

1. Decide and apply the version in the root workspace manifest.
2. Update dependencies on internal crates consistently.
3. Update `RELEASE_NOTES.md` with the actual version, format, and schema.
4. Run all quality gates.
5. Build the optimized binaries.

Create a package by passing the version explicitly:

```bash
bash scripts/package-release.sh 0.2.0
```

The script:

- builds `mdtree` and `mdtree-mcp` with `--locked`;
- stages the binaries and required notices;
- creates a reproducible tar archive using `SOURCE_DATE_EPOCH` when supplied;
- writes a SHA-256 checksum.

Do not rely on the script's fallback version; verify it matches the intended
release.

Verify the staged binary directory:

```bash
bash scripts/verify-package.sh \
  dist/mdtree-0.2.0-$(rustc -vV | awk '/host:/ {print $2}')
```

The verification imports an example, checks status and search, validates the
workspace, creates a backup, and confirms that `mdtree-mcp` is executable.

Review these files before publishing:

- `README.md`;
- `KNOWLEDGE_BASE.md`;
- `RELEASE_NOTES.md`;
- `LICENSE`;
- `COMMERCIAL-LICENSE.md`;
- `THIRD_PARTY_NOTICES.md`;
- generated archive and checksum.

## 14. Contribution conventions

Workspace-wide lint policy:

- unsafe Rust is forbidden;
- missing public documentation is warned;
- Clippy `all` and `pedantic` lints are enabled;
- formatting follows the checked-in rustfmt configuration.

For every change:

- preserve unrelated working-tree changes;
- keep domain rules out of delivery adapters;
- keep reads bounded when collections can be large;
- keep cursor and error contracts stable;
- require optimistic concurrency for existing-node writes;
- provide dry runs for consequential or destructive actions;
- avoid live network dependencies in normal tests;
- update examples, smoke tests, release notes, and this guide when user-visible
  behavior changes.

Suggested review checklist:

- Is the change in the correct crate?
- Is there one authoritative implementation of the invariant?
- Are canonical and derived data handled correctly?
- Is the operation transactional?
- Are read-only and read/write MCP surfaces correct?
- Are web mutations authenticated and version-checked?
- Are output and error contracts backward compatible?
- Are migration and snapshot implications covered?
- Do focused and workspace-wide tests pass?
- Can a newcomer discover the new workflow here?

## 15. Quick reference

### Important files

| Need | File or directory |
| --- | --- |
| Workspace members, versions, shared dependencies | `Cargo.toml` |
| Rust toolchain | `rust-toolchain.toml` |
| User installation | `README.md`, `build-and-install.sh` |
| Domain exports | `crates/mdtree-core/src/lib.rs` |
| Persistence API | `crates/mdtree-sqlite/src/lib.rs` |
| Schema migrations | `crates/mdtree-sqlite/migrations/` |
| CLI command definitions | `crates/mdtree-cli/src/lib.rs` |
| MCP startup flags | `crates/mdtree-mcp/src/main.rs` |
| MCP tools/resources | `crates/mdtree-mcp/src/lib.rs` |
| Web routes and server startup | `crates/mdtree-web/src/lib.rs` |
| Semantic evidence | `crates/mdtree-semantic/ACCEPTANCE.md` |
| Example workspaces | `examples/README.md` |
| Documentation smoke test | `scripts/smoke-docs.sh` |
| Release packaging | `scripts/package-release.sh` |
| Package verification | `scripts/verify-package.sh` |

### Environment variables

| Variable | Meaning |
| --- | --- |
| `MDTREE_WORKSPACE` | Default workspace path for CLI and MCP |
| `MDTREE_FALLBACK_WORKSPACE` | MCP fallback when no local `.mdtree` exists |
| `MDTREE_MCP_ALLOW_WRITE` | Enable MCP writes with `1`, `true`, or `yes` |
| `MDTREE_OLLAMA_URL` | Ollama base URL |
| `MDTREE_OLLAMA_MODEL` | Embedding model |
| `MDTREE_OLLAMA_TIMEOUT_SECONDS` | Provider timeout, default 60 seconds |
| `SOURCE_DATE_EPOCH` | Reproducible release archive timestamp |

### Common commands

```bash
# Build and validate the repository
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
bash scripts/smoke-docs.sh

# Inspect a workspace
mdtree status
mdtree tree
mdtree doctor
mdtree check

# Read before writing
mdtree --output json show NODE
mdtree context NODE --mode write

# Protect and transfer data
mdtree backup BACKUP.mdtree
mdtree export SNAPSHOT.json --format json

# Inspect semantic state
mdtree semantic-index status
mdtree search "QUERY" --mode lexical
```

### If changing X, inspect Y

| Change | Inspect first |
| --- | --- |
| Node identity, selectors, slugs | `mdtree-core/src/identity.rs`, `slug_generation.rs` |
| Tree navigation | `mdtree-core/src/ports.rs`, SQLite projections |
| Mutation semantics | `mdtree-sqlite/src/mutation_assembly.rs`, `store.rs` |
| Markdown content model | `mdtree-markdown/src/` |
| Search filters/ranking | `mdtree-core/src/search.rs`, `mdtree-sqlite/src/search.rs` |
| Semantic retrieval | `mdtree-semantic/src/`, `mdtree-sqlite/src/semantic.rs` |
| CLI behavior | `mdtree-cli/src/lib.rs` |
| MCP behavior | `mdtree-mcp/src/lib.rs` and adapter modules |
| Web behavior | `mdtree-web/src/`, then `assets/` |
| Database compatibility | migrations, snapshot tests, examples |

## Glossary

- **Canonical data:** authoritative workspace state that users intentionally
  mutate.
- **Derived data:** reproducible sections, indexes, inferred references, or
  embeddings built from canonical data.
- **Node selector:** an ID, slug, or canonical path used to resolve a node.
- **Canonical path:** deterministic root-to-node slug path.
- **Sibling order:** zero-based ordering among children of one parent.
- **Optimistic concurrency:** reject a write when the supplied observed version
  is no longer current.
- **Revision hash:** deterministic hash representing revision-relevant state.
- **Workspace revision:** workspace-wide change counter used to observe
  canonical changes.
- **Reference:** typed directed relation from one node to another target.
- **Section:** heading-oriented Markdown fragment used for navigation and
  search.
- **FTS:** SQLite full-text search over derived section documents.
- **Semantic profile:** provider, model, dimensions, metric, and input-format
  identity for compatible embeddings.
- **Opaque cursor:** continuation token that clients store and return without
  interpreting.
