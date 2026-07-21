#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
version=${1:-0.1.0}
target=$(rustc -vV | awk '/host:/ {print $2}')
name="mdtree-${version}-${target}"
stage="$root/dist/$name"

if [[ -e "$stage" || -e "$root/dist/$name.tar.gz" ]]; then
  echo "release destination already exists: $name" >&2
  exit 2
fi

cargo build --locked --release -p mdtree-cli -p mdtree-mcp
mkdir -p "$stage"
install -m 0755 "$root/target/release/mdtree" "$stage/mdtree"
install -m 0755 "$root/target/release/mdtree-mcp" "$stage/mdtree-mcp"
install -m 0644 "$root/LICENSE" "$stage/LICENSE"
install -m 0644 "$root/README.md" "$stage/README.md"
install -m 0644 "$root/RELEASE_NOTES.md" "$stage/RELEASE_NOTES.md"
install -m 0644 "$root/THIRD_PARTY_NOTICES.md" "$stage/THIRD_PARTY_NOTICES.md"
epoch=${SOURCE_DATE_EPOCH:-0}
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  -C "$root/dist" -cf - "$name" | gzip -n >"$root/dist/$name.tar.gz"
if command -v sha256sum >/dev/null; then
  sha256sum "$root/dist/$name.tar.gz" >"$root/dist/$name.tar.gz.sha256"
else
  shasum -a 256 "$root/dist/$name.tar.gz" >"$root/dist/$name.tar.gz.sha256"
fi
echo "$root/dist/$name.tar.gz"
