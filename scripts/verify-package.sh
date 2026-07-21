#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin_dir=${1:?packaged binary directory required}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

"$bin_dir/mdtree" --workspace "$work/northstar.mdtree" import "$root/examples/northstar-platform.snapshot.json" --format json >/dev/null
"$bin_dir/mdtree" --workspace "$work/northstar.mdtree" status --output json >/dev/null
"$bin_dir/mdtree" --workspace "$work/northstar.mdtree" search "domain events kafka" --output json >/dev/null
"$bin_dir/mdtree" --workspace "$work/northstar.mdtree" check --output json >/dev/null
"$bin_dir/mdtree" --workspace "$work/northstar.mdtree" backup "$work/backup.mdtree" >/dev/null
test -x "$bin_dir/mdtree-mcp"
