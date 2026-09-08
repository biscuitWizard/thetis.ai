#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
SRC=tools/miro-board-list/src/miro.rs
status=0
for dir in tools/miro-*/; do
 [ "$dir" = "tools/miro-board-list/" ] && continue
 if [ "${1:-}" = "--check" ]; then cmp -s "$SRC" "${dir}src/miro.rs" || { echo "out of sync: ${dir}src/miro.rs" >&2; status=1; }
 else
  cp "$SRC" "${dir}src/miro.rs"
  cp tools/miro-board-list/Cargo.toml "${dir}Cargo.toml"
  name="${dir#tools/}"; name="${name%/}"
  sed -i "s/name = \"miro-board-list\"/name = \"$name\"/" "${dir}Cargo.toml"
  echo "synced ${dir}src/miro.rs and Cargo.toml"
 fi
done
exit $status
