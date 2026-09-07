#!/usr/bin/env bash
# Run the ablation sweep: what is each retrieval mechanism actually worth?
#
# This is the headline measurement. It holds the corpus and the queries fixed and
# toggles one mechanism at a time, reporting each delta with a paired-bootstrap
# 95% CI so that an arm which merely looks better is labelled `ns` rather than
# announced as a win.
#
# Two corpora, because they answer different questions:
#
#   skills   our own 61 pinned cards / 36 queries. Free, instant, and too small
#            to resolve anything but a large effect -- which is itself a finding.
#   toolret  9,529 external tool docs / 1,634 queries from ToolRet (ACL 2025,
#            Apache-2.0). Big enough to have statistical power. Needs embeddings
#            on the first run; cached afterwards.
#
# Usage:
#   ./ablate.sh                  both corpora
#   ./ablate.sh --skills-only    skip the big corpus (no API cost, no download)
#   ./ablate.sh --toolret-only   skip the small one
#   ./ablate.sh --lexical        force BM25 everywhere (no key needed)
#   ./ablate.sh --refetch        rebuild the ToolRet corpus from HuggingFace
#
# Anything else is passed through to the binary.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="${RETRIEVAL_BENCH_WORK:-/opt/thetis/workspace/zero-retrieval-bench}"
CORPUS="$WORK/toolret-10k.json"
OUT="${ABLATE_OUT:-$WORK}"

skills=1
toolret=1
refetch=0
passthrough=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skills-only)  toolret=0; shift ;;
        --toolret-only) skills=0; shift ;;
        --refetch)      refetch=1; shift ;;
        -h|--help)      sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 0 ;;
        *)              passthrough+=("$1"); shift ;;
    esac
done

mkdir -p "$OUT"

# The key lives in the local config overlay, not the environment, because host
# env vars are deliberately not exposed to tools. Read it only if not already set
# and only if we might need it.
if [[ -z "${OPENROUTER_API_KEY:-}" && -n "${THETIS_LOCAL_CONFIG:-}" && -f "${THETIS_LOCAL_CONFIG}" ]]; then
    key="$(python3 -c "
import os, re
m = re.search(r'api_key\s*=\s*\"(sk-or-[^\"]+)\"', open(os.environ['THETIS_LOCAL_CONFIG']).read())
print(m.group(1) if m else '')
" 2>/dev/null || true)"
    [[ -n "$key" ]] && export OPENROUTER_API_KEY="$key"
fi

if [[ -z "${OPENROUTER_API_KEY:-}" && -z "${OPENAI_API_KEY:-}" ]]; then
    echo "note: no embeddings key found; the dense arms will fall back to BM25" >&2
    echo "      and every dense-vs-lexical comparison becomes meaningless." >&2
    echo "      Pass --lexical to say you meant that." >&2
fi

# Build once, in a scratch tree, lifting the real shipping source.
echo "==> building the harness (lifting real source)"
"$HERE/run.sh" --build-only
BIN="$WORK/target/debug/retrieval-bench"
[[ -x "$BIN" ]] || { echo "could not find the built binary at $BIN" >&2; exit 1; }

if [[ $skills -eq 1 ]]; then
    echo
    echo "======================================================================"
    echo " skills corpus: 61 pinned cards, 36 queries, limit 4"
    echo "======================================================================"
    "$BIN" ablate \
        --skills "$WORK/build/corpus/v1" \
        --limit 4 \
        --json "$OUT/ablation-skills.json" \
        "${passthrough[@]}"
    python3 "$HERE/plot-ablation.py" "$OUT/ablation-skills.json" \
        -o "$OUT/ablation-skills.svg" || true
fi

if [[ $toolret -eq 1 ]]; then
    if [[ $refetch -eq 1 || ! -f "$CORPUS" ]]; then
        echo
        echo "==> fetching the ToolRet corpus (cached under $WORK/hf-cache)"
        "$HERE/fetch-toolret.py" --out "$CORPUS" --tools 10000 --queries 2000
    fi
    echo
    echo "======================================================================"
    echo " toolret corpus: 9.5k docs, 1.6k queries, limit 10"
    echo "======================================================================"
    echo " (first dense run embeds ~11k texts; later runs read the cache)"
    "$BIN" ablate \
        --corpus "$CORPUS" \
        --limit 10 \
        --json "$OUT/ablation-toolret.json" \
        "${passthrough[@]}"
    python3 "$HERE/plot-ablation.py" "$OUT/ablation-toolret.json" \
        -o "$OUT/ablation-toolret.svg" || true
fi

echo
echo "results in $OUT"
