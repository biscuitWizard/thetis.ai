#!/usr/bin/env bash
# Checks the harness itself, not the ranker it measures.
#
# Why this exists: a benchmark that silently stops measuring is worse than no
# benchmark, because the chart keeps drawing a line. Every check here
# corresponds to a failure that actually happened and would otherwise have
# passed unnoticed:
#
#   1. `mod toolret;` was declared unconditionally while toolret.rs imports
#      `lifted::table`, which is feature-gated. Every revision predating the
#      group table failed to build -- 72 of 104 commits on main -- and the
#      runner reported them as "could not be measured" rather than as a bug in
#      the harness. The build without the feature is now checked explicitly.
#   2. run.sh did not copy corpus/ into the scratch build, so the binary found
#      no fixture and silently graded the live skills/ tree instead. The scores
#      looked plausible, which is what made it dangerous.
#   3. The pinned fixture drifts from skills/ whenever a brief is edited. That
#      is legitimate, but it must be a deliberate re-pin, not a surprise.
#
# Run before committing a change to this directory.

set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WORK="${RETRIEVAL_BENCH_WORK:-/opt/thetis/workspace/zero-retrieval-bench}/selftest"
FAIL=0

pass() { echo "  ok    $1"; }
fail() { echo "  FAIL  $1"; FAIL=$((FAIL + 1)); }

echo "retrieval-bench selftest"

# --- 1. the crate builds with and without the toolret feature ---------------
# Both configurations are real: a modern revision has a group table, an old one
# does not. Only the first was ever exercised before.
for feat in with without; do
    d="$WORK/$feat"
    rm -rf "$d"; mkdir -p "$d/src/lifted"
    cp "$HERE/Cargo.toml" "$d/"
    cp -r "$HERE/gold" "$d/"
    [[ -d "$HERE/corpus" ]] && cp -r "$HERE/corpus" "$d/"
    cp "$HERE/src"/*.rs "$d/src/"
    for f in skills skill_index skill_lint; do
        cp "$REPO/crates/thetis/src/$f.rs" "$d/src/lifted/"
    done

    args=()
    if [[ "$feat" == with ]]; then
        if ! python3 "$HERE/extract.py" \
            "$REPO/agents/agent-core/src/groups.rs" > "$d/src/lifted/table.rs" 2>/dev/null
        then
            fail "extract.py could not lift the group table"
            continue
        fi
        args=(--features toolret)
    fi

    if (cd "$d" && CARGO_TARGET_DIR="$WORK/target" \
        cargo build --quiet "${args[@]}" 2>&1 | grep -q '^error'); then
        fail "build $feat toolret"
        (cd "$d" && CARGO_TARGET_DIR="$WORK/target" \
            cargo build "${args[@]}" 2>&1 | grep '^error' | head -3 | sed 's/^/        /')
    else
        pass "build $feat toolret"
    fi
done

# --- 2. the runner grades the pinned corpus, not the live tree --------------
# Asserted on the runner's own output rather than by inspecting run.sh, so a
# future refactor that drops the copy is caught by behaviour.
if [[ -d "$HERE/corpus/v1" ]]; then
    out="$("$HERE/run.sh" --lexical 2>&1)"
    if grep -q 'pinned corpus/v1' <<<"$out"; then
        pass "default run uses the pinned fixture"
    else
        fail "default run did not use the pinned fixture"
        grep -E 'corpus=' <<<"$out" | sed 's/^/        /'
    fi

    # A gold id naming a skill absent from the corpus is unwinnable and drags
    # the mean down silently. The runner warns; here it must have nothing to
    # warn about.
    if grep -qE 'WARNING .* not in the corpus|WARNING unknown ids' <<<"$out"; then
        fail "gold set references ids missing from the pinned corpus"
        grep -E 'WARNING' <<<"$out" | cut -c1-100 | sed 's/^/        /'
    else
        pass "every gold id exists in the pinned corpus"
    fi
else
    fail "no pinned corpus at corpus/v1"
fi

# --- 3. the fixture matches the live tree ----------------------------------
# A warning, not a failure: drift is expected after editing a brief. It must be
# re-pinned deliberately, in its own commit, so the chart shows where the
# corpus moved.
if [[ -d "$HERE/corpus/v1" ]]; then
    if python3 "$HERE/pin-corpus.py" --from "$REPO/skills" \
        --out "$HERE/corpus/v1" --exclude torchship --check >/dev/null 2>&1
    then
        pass "pinned fixture matches skills/"
    else
        echo "  note  pinned fixture differs from skills/ (expected after a brief edit)"
        echo "        re-pin deliberately:  ./pin-corpus.py --from ../../skills \\"
        echo "                                --out corpus/v1 --exclude torchship"
    fi
fi

# --- 4. metric unit tests --------------------------------------------------
if (cd "$HERE" && CARGO_TARGET_DIR="$WORK/target" cargo test --quiet 2>&1 \
    | grep -qE '^test result: FAILED|panicked'); then
    fail "cargo test"
else
    pass "cargo test"
fi

echo
if [[ $FAIL -gt 0 ]]; then
    echo "$FAIL check(s) failed"
    exit 1
fi
echo "all checks passed"
