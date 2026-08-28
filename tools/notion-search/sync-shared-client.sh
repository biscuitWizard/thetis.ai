#!/usr/bin/env bash
# Keeps the shared Notion client identical across the notion-* tool crates.
#
# Every tool is a standalone cargo package — there is no workspace to hold a
# library — so `src/notion.rs` is duplicated eleven times. Duplication is
# tolerable; silent *divergence* is not, because a fix applied to one copy and
# not the others produces tools that disagree about error handling.
#
# notion-search holds the canonical copy. Edit that one, then run this.
#
#   ./sync-shared-client.sh          copy the canonical file over the others
#   ./sync-shared-client.sh --check  report drift without writing (for CI)
#
# Exits non-zero on drift under --check, or on a copy that cannot be written.
set -euo pipefail

cd "$(dirname "$0")/.."
canonical="notion-search/src/notion.rs"
[[ -f $canonical ]] || { echo "missing canonical file: $canonical" >&2; exit 1; }

check_only=false
[[ ${1:-} == --check ]] && check_only=true

drift=0
copies=0
for dir in notion-*/; do
  target="${dir}src/notion.rs"
  [[ $target == "$canonical" ]] && continue
  [[ -f $target ]] || { echo "missing: $target" >&2; drift=1; continue; }
  copies=$((copies + 1))

  if cmp -s "$canonical" "$target"; then
    continue
  fi

  drift=1
  if $check_only; then
    echo "DRIFT: $target differs from $canonical"
    diff -u "$canonical" "$target" | head -20 || true
  else
    cp "$canonical" "$target"
    echo "synced: $target"
  fi
done

if $check_only; then
  if [[ $drift -eq 0 ]]; then
    echo "all $copies copies match $canonical"
  else
    echo "run ./sync-shared-client.sh to fix" >&2
    exit 1
  fi
else
  echo "done; $copies copies now match $canonical"
fi
