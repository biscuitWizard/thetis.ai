#!/usr/bin/env bash
# Copies the canonical github.rs into every other github-* tool crate.
#
# Each tool is a standalone cargo package built for wasm32-wasip2, so there is
# no workspace to hold a shared library and the client is duplicated instead —
# the same arrangement as notion.rs across the notion-* group.
#
# github-whoami holds the canonical copy. Edit that one, then run this.
set -euo pipefail
cd "$(dirname "$0")/.."

SRC=tools/github-whoami/src/github.rs
test -f "$SRC" || { echo "missing $SRC" >&2; exit 1; }

for dir in tools/github-*/; do
  [ "$dir" = "tools/github-whoami/" ] && continue
  cp "$SRC" "${dir}src/github.rs"
  echo "synced ${dir}src/github.rs"
done
