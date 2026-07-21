# Example workspaces

Alongside the existing snapshot fixtures, this directory contains two portable,
realistic `.mdtree` databases:

- `mdtree-project.mdtree` is a static example of a project knowledge base
  shipped with software.
- `developer-workspace.mdtree` organizes projects, active and archived
  features, task-status relations, reusable LLM prompts, local inference notes,
  Linux kernel customizations, and VS Code practices.

The examples contain fictional or public project information and no secrets.
Open either directly:

```bash
mdtree --workspace examples/mdtree-project.mdtree
mdtree --workspace examples/developer-workspace.mdtree
```

Useful demonstrations:

```bash
mdtree --workspace examples/mdtree-project.mdtree search "SQLite FTS5"
mdtree --workspace examples/mdtree-project.mdtree show useful-commands
mdtree --workspace examples/developer-workspace.mdtree show t091-10-000-node-benchmark-generator
mdtree --workspace examples/developer-workspace.mdtree references t091-10-000-node-benchmark-generator
mdtree --workspace examples/developer-workspace.mdtree search "codebase orientation prompts"
mdtree --workspace examples/developer-workspace.mdtree show archived-features
```

`developer-workspace.mdtree` must pass `mdtree check` and match its versioned
fixture definition. Regenerate and validate it with:

```bash
cargo run -p mdtree-sqlite --example generate_example_workspaces
cargo run -p mdtree-cli -- --workspace examples/developer-workspace.mdtree check
cargo test -p mdtree-sqlite --test example_workspaces
```

Passing an output directory writes `developer-workspace.mdtree` there instead
of under the repository's `examples/` directory. The generator never modifies
the repository root `.mdtree` or `mdtree-project.mdtree`.

The generator uses stable node identities, canonical content, metadata, and
typed relations. SQLite's internal page representation and derived-record IDs
are implementation details; reproducibility means equivalent logical workspace
state rather than byte-identical database files.
