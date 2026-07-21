# MDTree 0.1.0

MDTree 0.1.0 provides a local-first SQLite workspace for strict hierarchical
Markdown knowledge. It includes deterministic navigation, weighted and scoped
FTS search, destination recommendation with ambiguity explanations, bounded
read/write context, typed references and backlinks, optimistic versioned
mutations, immutable history/diff/restore, integrity diagnostics, online
backup/guarded restore, and lossless JSON/Markdown snapshots.

Two binaries are shipped:

- `mdtree` — complete CLI read/write and maintenance surface.
- `mdtree-mcp` — read-only-by-default stdio MCP resources and tools, with an
  explicitly enabled local mutation surface.

Workspace format version 1 and schema version 5 are supported. Workspaces are
portable SQLite files, but only one writer should mutate a workspace at a time.
Authenticated or remotely exposed MCP writes, GUI, hosted collaboration, CRDT
sync, mandatory vector search, and file watching are outside v0.1. See
`docs/compatibility.md` and `README.md` for details.
