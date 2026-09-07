#!/usr/bin/env bash
# Synchronize the canonical RPG tool operation implementation into all 30
# standalone tool crates.
#
# Each tool remains a standalone wasm package. Keeping the shared source inside
# each tool's own tree means the existing aspect-tree build-cache key remains
# complete: changing canonical code changes every consumer tree after this sync.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
canonical="tools/rpg-roll"
src="$root/$canonical/src/lib.rs"
test -f "$src" || { echo "missing canonical $canonical/src/lib.rs" >&2; exit 1; }

seen=0
while IFS= read -r consumer; do
  [[ -z "$consumer" || "$consumer" == \#* ]] && continue
  test -f "$root/$consumer/Cargo.toml" || { echo "missing consumer $consumer" >&2; exit 1; }
  if [[ "$consumer" != "$canonical" ]]; then
    cp "$src" "$root/$consumer/src/lib.rs"
    echo "synced $consumer/src/lib.rs"
  fi
  seen=$((seen + 1))
done < "$root/rpg/consumers.txt"

[[ $seen -eq 30 ]] || { echo "expected 30 RPG consumers, found $seen" >&2; exit 1; }
