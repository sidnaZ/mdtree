#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
install_dir=${1:-"${HOME}/.local/bin"}

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required but was not found in PATH" >&2
  exit 1
fi

if ! command -v install >/dev/null 2>&1; then
  echo "install is required but was not found in PATH" >&2
  exit 1
fi

cd "$repo_root"
cargo build --release --locked -p mdtree-cli -p mdtree-mcp

install -d "$install_dir"
install -m 0755 "$repo_root/target/release/mdtree" "$install_dir/mdtree"
install -m 0755 "$repo_root/target/release/mdtree-mcp" "$install_dir/mdtree-mcp"

echo "Installed mdtree and mdtree-mcp in $install_dir"

if [[ ":$PATH:" != *":$install_dir:"* ]]; then
  echo "Warning: $install_dir is not in PATH." >&2
  echo "Add it to the current shell with:" >&2
  printf '  export PATH="%s:$PATH"\n' "$install_dir" >&2
  echo "Persist it for future Bash sessions with:" >&2
  printf '  echo '\''export PATH="%s:$PATH"'\'' >> "$HOME/.bashrc"\n' "$install_dir" >&2
fi
