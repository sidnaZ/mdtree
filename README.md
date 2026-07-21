# MDTree

MDTree is a local-first tool for structured tree data. Humans explore it
visually, AI agents work with it through MCP, and computer systems integrate
with it through the CLI or an API—all against the same portable `.mdtree`
workspace.

![MDTree browser showing a structured project tree](browser.png)

## Build and connect

MDTree requires Rust 1.88 or newer.

```bash
git clone https://github.com/sidnaZ/mdtree-ui.git
cd mdtree-ui
cargo build --release --locked
```

Install the CLI and MCP server:

```bash
cargo install --locked --path crates/mdtree-cli
cargo install --locked --path crates/mdtree-mcp
```

Register the MCP server with Codex and Claude Code, replacing
`/path/to/workspaces` with the directory that contains the workspaces agents
may access:

```bash
codex mcp add mdtree -- mdtree-mcp --allow-write --allow-workspace-switch --workspace-root /path/to/workspaces
claude mcp add --transport stdio --scope user mdtree -- mdtree-mcp --allow-write --allow-workspace-switch --workspace-root /path/to/workspaces
```

## License and contact

MDTree is dual-licensed under the [GNU Affero General Public License v3.0](LICENSE)
and a separate [commercial license](COMMERCIAL-LICENSE.md). Use MDTree freely
under the AGPL-3.0, including for commercial purposes, as long as you comply
with its terms (notably, publishing source for any modified version you run as
a network service). If that doesn't work for you—for example, embedding
MDTree in closed-source software or hosting a modified version without
releasing your changes—a commercial license is available. For commercial
licensing, questions, or feedback, [contact the maintainers](https://github.com/sidnaZ/mdtree/issues).
