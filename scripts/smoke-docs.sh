#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo run --quiet -p mdtree-cli -- --help >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/northstar.mdtree" import "$root/examples/northstar-platform.snapshot.json" --format json >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/northstar.mdtree" status --output json >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/northstar.mdtree" search "domain events kafka" --output json >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/northstar.mdtree" browse architecture <<< $'1\n\nq\n' | grep -F "Architecture Decisions" >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/northstar.mdtree" check --output json >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/northstar.mdtree" backup "$work/backup.mdtree" >/dev/null

cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" init "Northstar Platform" >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" create northstar-platform "Architecture" >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" create architecture "Architecture Decisions" --content $'# Architecture Decisions\n\nRecords decisions that affect multiple services.\n\n## Required sections\n\n- Context\n- Decision\n- Alternatives\n- Consequences' >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" create northstar-platform "Services" >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" create services "Service Catalog" >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" tree >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" show architecture-decisions | grep -F "Required sections" >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" check --output json >/dev/null
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" browse <<< 'q' | grep -F "Northstar Platform" >/dev/null

browse_log="$work/browse-ui.log"
cargo run --quiet -p mdtree-cli -- --workspace "$work/example.mdtree" browse-ui --no-open >"$browse_log"
browse_url=$(tr -d '\r\n' <"$browse_log")
[ -n "$browse_url" ]
curl -sf "$browse_url/" | grep -qi "<title>MDTree</title>"
session=$(curl -sf "$browse_url/api/session")
printf '%s' "$session" | grep -q '"session_credential"'
credential=$(printf '%s' "$session" | sed -n 's/.*"session_credential":"\([^"]*\)".*/\1/p')
curl -sf -X POST -H "x-mdtree-session: $credential" "$browse_url/api/stop" >/dev/null
