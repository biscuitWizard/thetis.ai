#!/usr/bin/env bash
# Produce the two published infographics and one datapoint, into a staging
# directory that the kernel commits to the `bench-results` branch.
#
# This is what runs when you press "push to origin" on /admin. It is meant to be
# boring and fast: the harness is already built, the embedding vectors are
# already cached, so a warm run is a few seconds. Everything it writes lands in
# one directory and nothing outside that directory is published.
#
#   tool-routing.svg     how well tag routing attaches the right tool groups,
#                        and what dense retrieval would add
#   skill-retrieval.svg  how well the ranker surfaces the right skill card
#   datapoint.jsonl      one line of numbers, appended to results.jsonl on the
#                        branch so the series grows with every publish
#   README.md            what the branch is, regenerated each time
#
# Usage:
#   ./publish-graphs.sh --staging DIR   write there (required)
#   ./publish-graphs.sh --lexical       BM25 only; no key needed, labels the run
#
# Exit 0 with an empty staging directory means "nothing to publish" and is not
# a failure: a checkout with no built harness or no corpus is a normal state.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="${RETRIEVAL_BENCH_WORK:-/opt/thetis/workspace/zero-retrieval-bench}"
CORPUS="${RETRIEVAL_BENCH_CORPUS:-$WORK/toolret-10k.json}"
CACHE="$WORK/cache"
BIN="$WORK/target/debug/retrieval-bench"

STAGING=""
lexical=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --staging) STAGING="$2"; shift 2 ;;
        --lexical) lexical=1; shift ;;
        -h|--help) sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$STAGING" ]]; then
    echo "publish-graphs: --staging DIR is required" >&2
    exit 2
fi

# Start from empty. A leftover chart from a previous run would otherwise be
# republished as though it were current, which is the worst failure available
# here: a plausible graph with stale numbers.
rm -rf "$STAGING"
mkdir -p "$STAGING"

say() { echo "publish-graphs: $*" >&2; }

# The key lives in the local config overlay because host env vars are not
# exposed to tools. Without it the dense arms silently become BM25, so we label
# the run rather than let the chart imply a dense measurement.
if [[ -z "${OPENROUTER_API_KEY:-}" && -n "${THETIS_LOCAL_CONFIG:-}" && -f "${THETIS_LOCAL_CONFIG}" ]]; then
    key="$(grep -oE 'api_key[[:space:]]*=[[:space:]]*"sk-or-[^"]+"' "$THETIS_LOCAL_CONFIG" 2>/dev/null \
           | head -1 | sed -E 's/.*"(sk-or-[^"]+)".*/\1/')"
    [[ -n "$key" ]] && export OPENROUTER_API_KEY="$key"
fi

mode="dense"
if (( lexical )); then
    mode="lexical (forced)"
elif [[ -z "${OPENROUTER_API_KEY:-}" && -z "${OPENAI_API_KEY:-}" ]]; then
    say "no embeddings key; measuring the BM25 fallback and labelling it so"
    lexical=1
    mode="lexical (no key available)"
fi

lexflag=()
(( lexical )) && lexflag=(--lexical)

# Build if needed. Publishing should not be the thing that discovers the harness
# does not compile, but neither should it fail the whole publish.
if [[ ! -x "$BIN" ]]; then
    say "harness not built; building"
    if ! "$HERE/run.sh" --build-only >"$STAGING/.build.log" 2>&1; then
        say "harness will not build; publishing no results"
        say "$(tail -5 "$STAGING/.build.log" 2>/dev/null)"
        rm -rf "$STAGING"; mkdir -p "$STAGING"
        exit 0
    fi
fi
if [[ ! -x "$BIN" ]]; then
    say "no binary at $BIN; publishing no results"
    rm -rf "$STAGING"; mkdir -p "$STAGING"
    exit 0
fi
rm -f "$STAGING/.build.log"

# --- measure -----------------------------------------------------------------
# Each measurement is optional. A missing ToolRet corpus is common (3.5 MB, not
# committed), and the skills ablation alone still makes a publishable chart.

skills_json="$STAGING/.ablation-skills.json"
route_json="$STAGING/.routescale.json"
toolret_json="$STAGING/.ablation-toolret.json"

say "ablating the skill ranker on the pinned corpus"
if ! "$BIN" ablate \
        --skills "$WORK/build/corpus/v1" \
        --limit 4 --cache "$CACHE" \
        "${lexflag[@]}" \
        --json "$skills_json" >"$STAGING/.skills.log" 2>&1; then
    say "skills ablation failed:"
    say "$(tail -5 "$STAGING/.skills.log" 2>/dev/null)"
    rm -f "$skills_json"
fi

if [[ -f "$CORPUS" ]]; then
    say "routing $(basename "$CORPUS") through the shipped tag matcher"
    if ! "$BIN" routescale \
            --corpus "$CORPUS" --cache "$CACHE" \
            "${lexflag[@]}" \
            --json "$route_json" >"$STAGING/.route.log" 2>&1; then
        say "routescale failed:"
        say "$(tail -5 "$STAGING/.route.log" 2>/dev/null)"
        rm -f "$route_json"
    fi

    say "ablating the ranker on the large external corpus"
    if ! "$BIN" ablate \
            --corpus "$CORPUS" \
            --limit 10 --cache "$CACHE" \
            "${lexflag[@]}" \
            --json "$toolret_json" >"$STAGING/.toolret.log" 2>&1; then
        say "large-corpus ablation failed:"
        say "$(tail -5 "$STAGING/.toolret.log" 2>/dev/null)"
        rm -f "$toolret_json"
    fi
else
    say "no ToolRet corpus at $CORPUS; charts will cover the pinned corpus only"
    say "(build it with ./fetch-toolret.py, ~3.5 MB, needs network)"
fi

if [[ ! -f "$skills_json" && ! -f "$route_json" ]]; then
    say "every measurement failed; publishing nothing"
    rm -rf "$STAGING"; mkdir -p "$STAGING"
    exit 0
fi

# --- draw --------------------------------------------------------------------

args=()
[[ -f "$route_json"   ]] && args+=(--routescale "$route_json")
[[ -f "$skills_json"  ]] && args+=(--skills "$skills_json")
[[ -f "$toolret_json" ]] && args+=(--toolret "$toolret_json")

say "drawing"
if ! python3 "$HERE/infographic.py" "${args[@]}" --outdir "$STAGING" >"$STAGING/.draw.log" 2>&1; then
    say "drawing failed:"
    say "$(tail -20 "$STAGING/.draw.log" 2>/dev/null)"
    rm -rf "$STAGING"; mkdir -p "$STAGING"
    exit 0
fi

# --- datapoint and README ----------------------------------------------------

if ! python3 "$HERE/datapoint.py" \
        --staging "$STAGING" --mode "$mode" \
        --repo "$HERE/../.." >"$STAGING/.dp.log" 2>&1; then
    say "datapoint assembly failed (charts still publishable):"
    say "$(tail -10 "$STAGING/.dp.log" 2>/dev/null)"
fi

# The intermediate JSON and logs are inputs, not artifacts. Dotfiles are dropped
# wholesale so a new log added above cannot accidentally get published.
find "$STAGING" -maxdepth 1 -name '.*' -type f -delete

say "staged: $(cd "$STAGING" && ls | tr '\n' ' ')"
exit 0
