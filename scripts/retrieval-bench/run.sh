#!/usr/bin/env bash
#
# Run the retrieval benchmark against the working tree or any past revision.
#
#   ./run.sh                          measure the working tree
#   ./run.sh --verbose                per-case detail
#   ./run.sh --rev 5bc9bab            measure one past revision
#   ./run.sh --rev HEAD~5..HEAD       measure a range, one datapoint each
#   ./run.sh --last 10                measure the last 10 commits
#   ./run.sh --out results.jsonl      append datapoints to a file
#
# How a past revision is measured, and why it is done this way:
#
# The harness is INJECTED into a detached worktree of the old commit rather than
# checked out with it. The gold sets, the metric code and the runner therefore
# stay fixed across every point on the chart, while the things under measurement
# -- the skill corpus, the ranker, the group table, the tag lists -- are whatever
# that commit had. That is the only arrangement in which the numbers are
# comparable: if the gold set travelled with the checkout, every point would be
# scored against a different exam and the series would mean nothing.
#
# The cost of the choice is that a revision predating a source file the harness
# lifts cannot be measured with it. Rather than fail, the injection degrades:
# a missing group table drops ToolRet and still reports SkillRet, and a revision
# with no lifted ranker at all is skipped with a reason. Datapoints produced this
# way are marked `backfilled: true`.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WORK="${RETRIEVAL_BENCH_WORK:-/opt/thetis/workspace/zero-retrieval-bench}"
CACHE="$WORK/cache"

REV=""
LAST=""
OUT=""
PASSTHRU=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rev)  REV="$2"; shift 2 ;;
        --last) LAST="$2"; shift 2 ;;
        --out)  OUT="$2"; shift 2 ;;
        *)      PASSTHRU+=("$1"); shift ;;
    esac
done

mkdir -p "$CACHE"
[[ -n "$OUT" ]] && mkdir -p "$(dirname "$OUT")"

# The four files the harness lifts. Relative to the repo root, and each paired
# with where it lands in the crate.
lift_into() {
    local src_root="$1" dest="$2" ok=0
    mkdir -p "$dest/lifted"

    # skills.rs and skill_index.rs are the measurement: without both there is
    # nothing to benchmark.
    local f
    for f in skills skill_index; do
        if [[ -f "$src_root/crates/thetis/src/$f.rs" ]]; then
            cp "$src_root/crates/thetis/src/$f.rs" "$dest/lifted/$f.rs"
            ok=$((ok + 1))
        fi
    done

    if [[ $ok -lt 2 ]]; then
        echo "  no liftable ranker at this revision ($ok/2 core files); skipping" >&2
        return 1
    fi

    # skill_lint.rs postdates skills.rs, and skills.rs calls into it in exactly
    # one place -- `crate::skill_lint::body_diagnostics`, which lints bodies and
    # has nothing to do with ranking. Rather than lose every revision older than
    # the linter, stub that one function when the file is absent. The stub is
    # never reached: the harness discovers and ranks, it does not lint. Without
    # this the series would start at the linter's commit for no reason connected
    # to retrieval.
    if [[ -f "$src_root/crates/thetis/src/skill_lint.rs" ]]; then
        cp "$src_root/crates/thetis/src/skill_lint.rs" "$dest/lifted/skill_lint.rs"
    else
        cat > "$dest/lifted/skill_lint.rs" <<'STUB'
//! STUB, written by run.sh: this revision predates crates/thetis/src/skill_lint.rs.
//!
//! Satisfies the single reference skills.rs makes into the linter. Body linting
//! is not measured by this benchmark, so an empty result changes no number.
use crate::skills::{Diagnostic, Skill, SkillTree};

pub fn body_diagnostics(_tree: &SkillTree, _skill: &Skill) -> Vec<Diagnostic> {
    Vec::new()
}
STUB
        echo "  note: no skill_lint.rs at this revision; stubbed" >&2
    fi

    # The group table is optional: it postdates the ranker.
    local groups="$src_root/agents/agent-core/src/groups.rs"
    if [[ -f "$groups" ]] && python3 "$HERE/extract.py" "$groups" > "$dest/lifted/table.rs" 2>/dev/null; then
        return 0
    fi
    rm -f "$dest/lifted/table.rs"
    return 2   # ranker yes, table no
}

# Build and run in a scratch copy of the crate, so an injected lift never dirties
# the checked-in one and two revisions cannot race over the same target dir.
measure() {
    local src_root="$1" label="$2" backfill="$3"
    local build="$WORK/build"

    rm -rf "$build"
    mkdir -p "$build"
    cp "$HERE/Cargo.toml" "$build/"
    cp -r "$HERE/gold" "$build/"
    mkdir -p "$build/src"
    cp "$HERE/src"/*.rs "$build/src/"

    local features=()
    lift_into "$src_root" "$build/src"
    case $? in
        0) features=(--features toolret) ;;
        1) return 1 ;;
        2) echo "  note: no group table at this revision; SkillRet only" >&2 ;;
    esac

    # Share one target dir across revisions: the dependency graph is identical,
    # so only the harness itself recompiles and a 12-point backfill costs one
    # cold build instead of twelve.
    local args=(--root "$src_root" --cache "$CACHE")
    [[ -n "$OUT" ]] && args+=(--json "$build/point.json")

    RETRIEVAL_BENCH_BACKFILL="$backfill" \
    CARGO_TARGET_DIR="$WORK/target" \
        cargo run --quiet --manifest-path "$build/Cargo.toml" \
        "${features[@]}" -- "${args[@]}" "${PASSTHRU[@]+"${PASSTHRU[@]}"}"
    local rc=$?

    if [[ $rc -eq 0 && -n "$OUT" && -f "$build/point.json" ]]; then
        # One JSON object per line: appendable, diffable, and readable by the
        # plot script without parsing a growing array.
        python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])), separators=(",",":")))' \
            "$build/point.json" >> "$OUT"
        echo "  appended to $OUT" >&2
    fi
    return $rc
}

# --- working tree -----------------------------------------------------------

if [[ -z "$REV" && -z "$LAST" ]]; then
    measure "$REPO" "working tree" ""
    exit $?
fi

# --- past revisions ---------------------------------------------------------

if [[ -n "$LAST" ]]; then
    mapfile -t REVS < <(git -C "$REPO" rev-list --reverse -n "$LAST" HEAD)
elif [[ "$REV" == *..* ]]; then
    mapfile -t REVS < <(git -C "$REPO" rev-list --reverse "$REV")
else
    mapfile -t REVS < <(git -C "$REPO" rev-parse "$REV")
fi

if [[ ${#REVS[@]} -eq 0 ]]; then
    echo "no revisions matched" >&2
    exit 1
fi

echo "measuring ${#REVS[@]} revision(s)" >&2
TREE="$WORK/worktree"
FAILED=0

# Remove the scratch worktree however this script ends, interrupt included.
# Without the trap, a Ctrl-C mid-backfill leaves a registered worktree behind,
# and the next `git worktree add` at the same path fails until someone prunes it
# by hand.
cleanup_tree() {
    git -C "$REPO" worktree remove --force "$TREE" >/dev/null 2>&1
    rm -rf "$TREE"
    git -C "$REPO" worktree prune >/dev/null 2>&1
}
trap cleanup_tree EXIT INT TERM

for rev in "${REVS[@]}"; do
    short="${rev:0:8}"
    echo >&2
    echo "=== $short $(git -C "$REPO" log -1 --format=%s "$rev" | cut -c1-60)" >&2

    # A detached worktree, not a checkout: the real checkout is left untouched,
    # which matters because other conversations share this repo.
    cleanup_tree
    if ! git -C "$REPO" worktree add --detach --quiet "$TREE" "$rev" 2>&1; then
        echo "  could not create a worktree at $short; skipping" >&2
        FAILED=$((FAILED + 1))
        continue
    fi

    measure "$TREE" "$short" "1" || FAILED=$((FAILED + 1))
done

echo >&2
if [[ $FAILED -gt 0 ]]; then
    echo "$FAILED of ${#REVS[@]} revision(s) could not be measured" >&2
fi
[[ -n "$OUT" ]] && echo "datapoints in $OUT: $(wc -l < "$OUT")" >&2
exit 0
