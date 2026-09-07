#!/usr/bin/env bash
# Keeps the shared mooR client identical across the moo-* tool crates.
#
# Each moo-* tool is a standalone cargo package — there is no workspace to
# hold a library — so `src/moo.rs` is duplicated into every one of them.
# Duplication is tolerable; silent *divergence* is not, because a fix applied
# to one copy and not the others produces tools that disagree about error
# handling and config resolution.
#
# moo-server-info holds the canonical copy. Edit that one, then run this.
#
#   ./sync-shared-client.sh          copy the canonical file over the others
#   ./sync-shared-client.sh --check  report drift without writing (for CI)
#
# There is only one moo-* crate today. This script is here so the next one
# starts from day one following the same rule the notion-* and bq-* families
# use (see notion-search/sync-shared-client.sh and its
# skills/notion-workspace/maintaining-the-tools/SKILL.md for the pattern this
# copies) — add a crate under tools/moo-*/, drop src/moo.rs in with this
# script, and it is picked up automatically.
#
# Exits non-zero on drift under --check, or on a copy that cannot be written.
set -euo pipefail

cd "$(dirname "$0")/.."
canonical="moo-server-info/src/moo.rs"
[[ -f $canonical ]] || { echo "missing canonical file: $canonical" >&2; exit 1; }

check_only=false
[[ ${1:-} == --check ]] && check_only=true

drift=0
copies=0
for dir in moo-*/; do
  target="${dir}src/moo.rs"
  [[ $target == "$canonical" ]] && continue
  if [[ ! -f $target ]]; then
    if $check_only; then
      echo "missing: $target" >&2; drift=1; continue
    fi
    cp "$canonical" "$target"
    echo "created: $target"
  fi
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
