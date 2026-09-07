#!/usr/bin/env python3
"""Reduce a run's JSON outputs to one datapoint line, and write the branch README.

The datapoint is what makes `bench-results` a series rather than a snapshot: one
line per publish, appended by the kernel to `results.jsonl`. It records the
headline numbers and enough provenance to know what produced them, because a
number whose corpus and mode are unknown cannot be compared with the line above
it.

Deliberately flat: nested JSON in a JSONL series is painful to plot, so every
field is a scalar with a prefixed name.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from statistics import median

SCHEMA = 2


def git(repo, *args):
    try:
        out = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True, text=True, timeout=30,
        )
        return out.stdout.strip() if out.returncode == 0 else ""
    except Exception:
        return ""


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return None


def arm(doc, name):
    if not doc:
        return None
    for a in doc.get("arms", []):
        if a.get("arm") == name:
            return a
    return None


def strategy(doc, name):
    if not doc:
        return None
    for s in doc.get("result", {}).get("strategies", []):
        if s.get("strategy") == name:
            return s
    return None


def corpus_label(doc):
    """A machine-independent name for the corpus a run graded.

    The harness records whatever path it was handed, which is a scratch
    directory. `corpus/v1` is the pinned fixture and the only one whose numbers
    are meant to be comparable across runs, so it gets a stable name; anything
    else keeps its basename and is thereby visibly not the pinned corpus.
    """
    raw = str(doc.get("source") or doc.get("corpus") or "")
    if "corpus/v1" in raw:
        return "pinned corpus/v1"
    head = raw.split(" + ")[0].strip()
    return os.path.basename(head.rstrip("/")) or "unknown"


# Every arm records the same four quality metrics, and a series is only useful if
# it carries all of them: nDCG can stay flat while hit@1 moves, which is exactly
# what happens on our own corpus, so recording nDCG alone would hide a real change.
ARM_FIELDS = {
    "ndcg_at_k": "ndcg",
    "hit_at_1": "hit1",
    "mrr": "mrr",
    "recall_at_k": "recall",
    "vs_baseline": "delta",
    "p_value": "p",
}


def flatten(point, prefix, src, fields):
    """Copy `fields` out of `src` under `prefix`, skipping what is absent."""
    if not src:
        return
    for key, out in fields.items():
        if key in src and src[key] is not None:
            point[f"{prefix}_{out}"] = src[key]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--staging", required=True)
    ap.add_argument("--mode", default="unknown")
    ap.add_argument("--repo", default=".")
    args = ap.parse_args()

    staging = Path(args.staging)
    skills = load(staging / ".ablation-skills.json")
    route = load(staging / ".routescale.json")
    toolret = load(staging / ".ablation-toolret.json")

    if not skills and not route:
        print("no measurements to record", file=sys.stderr)
        return 1

    repo = args.repo
    commit = git(repo, "rev-parse", "HEAD")
    point = {
        "schema": SCHEMA,
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "commit": commit,
        "commit_short": commit[:12],
        "commit_date": git(repo, "log", "-1", "--format=%cI"),
        "subject": git(repo, "log", "-1", "--format=%s"),
        "mode": args.mode,
    }

    # --- skill retrieval, our own pinned corpus ---
    if skills:
        # Normalised to a stable label: this field decides whether two rows are
        # comparable, so an absolute scratch path -- which differs per machine
        # and per checkout -- would report a corpus change that never happened.
        point["skills_corpus"] = corpus_label(skills)
        point["skills_docs"] = skills.get("docs", 0)
        point["skills_queries"] = skills.get("queries", 0)
        point["skills_limit"] = skills.get("limit", 0)
        point["skills_baseline"] = skills.get("baseline", "")
        for name, tag in (("dense", "dense"), ("bm25", "bm25"), ("fusion-w0.7", "fusion07")):
            flatten(point, f"skills_{tag}", arm(skills, name), ARM_FIELDS)

    # --- skill retrieval, large external corpus ---
    if toolret:
        point["ext_docs"] = toolret.get("docs", 0)
        point["ext_queries"] = toolret.get("queries", 0)
        point["ext_limit"] = toolret.get("limit", 0)
        for name, tag in (("dense", "dense"), ("bm25", "bm25"), ("fusion-w0.7", "fusion07")):
            flatten(point, f"ext_{tag}", arm(toolret, name), ARM_FIELDS)

    # --- tool routing ---
    if route:
        r = route.get("result", {})
        point["route_queries"] = r.get("queries", 0)
        point["route_groups"] = r.get("groups", 0)
        point["route_tools"] = r.get("tools", 0)
        point["route_unlabelled"] = r.get("unlabelled", 0)
        point["route_threshold"] = r.get("shipped_threshold", 0)
        point["route_best_f1"] = r.get("best_f1", 0)
        point["route_labels"] = route.get("labels", "")
        flatten(point, "route_shipped", r.get("at_shipped"), {
            "recall": "recall",
            "specificity": "specificity",
            "f1": "f1",
            "mean_attached": "groups_per_query",
            "unrouted": "unrouted",
        })
        for name, tag in (
            ("tags (shipped)", "tags"),
            ("dense top-2", "dense2"),
            ("tags, dense fallback", "hybrid"),
        ):
            flatten(point, f"route_{tag}", strategy(route, name), {
                "f1": "f1",
                "recall": "recall",
                "specificity": "specificity",
                "unrouted": "unrouted",
            })

        # Per-family agreement. A single aggregate F1 can be carried by one
        # large family; this records how many of the 35 independent benchmarks
        # move the same way, which is the part worth watching over time.
        fams = [s for s in (r.get("by_subset") or []) if s.get("best_f1") is not None]
        if fams:
            point["route_families"] = len(fams)
            point["route_families_improved"] = sum(
                1 for s in fams if s["best_f1"] - s["tags_f1"] > 0.001)
            point["route_families_regressed"] = sum(
                1 for s in fams if s["best_f1"] - s["tags_f1"] < -0.001)
            point["route_family_median_gain"] = round(median(
                s["best_f1"] - s["tags_f1"] for s in fams), 6)
            point["route_family_worst_gain"] = round(min(
                s["best_f1"] - s["tags_f1"] for s in fams), 6)

    with open(staging / "datapoint.jsonl", "w") as f:
        f.write(json.dumps(point, sort_keys=True) + "\n")

    write_readme(staging, point, skills, route, toolret)
    print(f"wrote datapoint.jsonl ({len(point)} fields) and README.md")
    return 0


def write_readme(staging, point, skills, route, toolret):
    """The branch's front page. Regenerated every publish, so it always
    describes the charts sitting next to it."""

    def num(key, fmt="{:.4f}", dash="—"):
        v = point.get(key)
        return fmt.format(v) if isinstance(v, (int, float)) else dash

    lines = [
        "# Retrieval benchmark results",
        "",
        "Published by Thetis when trunk is published from `/admin`. Two questions,",
        "two charts, and an append-only series of the numbers behind them.",
        "",
        "| | |",
        "|---|---|",
        "| ![skill retrieval](skill-retrieval.svg) | ![tool routing](tool-routing.svg) |",
        "",
        "- **[skill-retrieval.svg](skill-retrieval.svg)** — how well the ranker",
        "  surfaces the right skill card for a query, and what each mechanism in it",
        "  (dense embeddings, BM25, fusion, parent absorption, candidate pool) is",
        "  actually worth.",
        "- **[tool-routing.svg](tool-routing.svg)** — how well tag matching attaches",
        "  the right tool groups to a request, and what dense retrieval would add.",
        "- **[results.jsonl](results.jsonl)** — one JSON object per publish.",
        "",
        f"Latest run: `{point.get('commit_short', '?')}` at",
        f"{point.get('measured_at', '?')}, mode **{point.get('mode', '?')}**.",
        "",
        "## What the latest run says",
        "",
    ]

    if route:
        lines += [
            "### Tool routing",
            "",
            f"On {point.get('route_queries', 0):,} queries against "
            f"{point.get('route_tools', 0):,} tool documents in "
            f"{point.get('route_groups', 0)} groups:",
            "",
            "| strategy | F1 | recall | specificity | routed nothing |",
            "|---|---|---|---|---|",
        ]
        for label, tag in (
            ("tags (shipped)", "tags"),
            ("dense top-2", "dense2"),
            ("tags + dense fallback", "hybrid"),
        ):
            if f"route_{tag}_f1" in point:
                lines.append(
                    f"| {label} | {num(f'route_{tag}_f1')} | "
                    f"{num(f'route_{tag}_recall')} | "
                    f"{num(f'route_{tag}_specificity')} | "
                    f"{point.get(f'route_{tag}_unrouted', '—')} |"
                )
        if "route_families" in point:
            lines += [
                "",
                f"Split across the {point['route_families']} dataset families that "
                f"make up the benchmark, adding the embedding call improves "
                f"{point['route_families_improved']} and regresses "
                f"{point['route_families_regressed']}; median gain "
                f"{num('route_family_median_gain')} F1, worst family "
                f"{num('route_family_worst_gain')}. Each family has its own tool "
                "vocabulary, so agreement across them is stronger evidence than the "
                "pooled number.",
            ]
        lines += [
            "",
            "The labels are derived: the dataset's per-tool relevance judgements",
            "mapped through group buckets this harness synthesised. That makes the",
            "absolute F1 a property of the bucketing as much as of the routing, so",
            "compare strategies against each other, not against 1.0.",
            "",
        ]

    if skills:
        lines += [
            "### Skill retrieval",
            "",
            f"Pinned corpus: {point.get('skills_docs', 0)} cards, "
            f"{point.get('skills_queries', 0)} queries, "
            f"nDCG@{point.get('skills_limit', 0)}.",
            "",
            "| arm | nDCG | hit@1 | vs baseline |",
            "|---|---|---|---|",
        ]
        for label, tag in (("dense", "dense"), ("bm25", "bm25"), ("fusion w0.7", "fusion07")):
            if f"skills_{tag}_ndcg" in point:
                d = point.get(f"skills_{tag}_delta")
                p = point.get(f"skills_{tag}_p")
                delta = "baseline" if d in (None, 0) and tag == point.get("skills_baseline") \
                    else (f"{d:+.4f}" + (" ns" if isinstance(p, float) and p >= 0.05 else "")
                          if isinstance(d, float) else "—")
                lines.append(
                    f"| {label} | {num(f'skills_{tag}_ndcg')} | "
                    f"{num(f'skills_{tag}_hit1')} | {delta} |"
                )
        lines.append("")

        if toolret:
            lines += [
                f"Same ranker on {point.get('ext_docs', 0):,} external documents and "
                f"{point.get('ext_queries', 0):,} queries, which is where small effects",
                "become resolvable:",
                "",
                "| arm | nDCG | vs baseline |",
                "|---|---|---|",
            ]
            for label, tag in (("dense", "dense"), ("bm25", "bm25"), ("fusion w0.7", "fusion07")):
                if f"ext_{tag}_ndcg" in point:
                    d = point.get(f"ext_{tag}_delta")
                    p = point.get(f"ext_{tag}_p")
                    delta = (f"{d:+.4f}" + (" ns" if isinstance(p, float) and p >= 0.05 else "")
                             if isinstance(d, float) and d != 0 else "baseline")
                    lines.append(f"| {label} | {num(f'ext_{tag}_ndcg')} | {delta} |")
            lines.append("")

    lines += [
        "## How to read the series",
        "",
        "`results.jsonl` is append-only and flat — every field is a scalar, so it",
        "loads straight into a dataframe:",
        "",
        "```python",
        "import pandas as pd",
        "df = pd.read_json('results.jsonl', lines=True)",
        "df.plot(x='measured_at', y=['route_tags_f1', 'route_hybrid_f1'])",
        "```",
        "",
        "Two fields decide whether two rows are comparable: `mode` (dense or a BM25",
        "fallback) and `skills_corpus`. A row measured without an embeddings key is",
        "not a datapoint about dense retrieval, and the chart generator refuses to",
        "draw a delta across such a change rather than implying one.",
        "",
        "## Reproducing this",
        "",
        "```sh",
        "scripts/retrieval-bench/ablate.sh              # the sweep, both corpora",
        "scripts/retrieval-bench/publish-graphs.sh --staging /tmp/bench",
        "```",
        "",
        "The harness lifts the real shipping ranker out of",
        "`crates/thetis/src/skill_index.rs` at build time rather than reimplementing",
        "it, so an arm that passes here exercises the code that runs in production.",
        "See `scripts/retrieval-bench/README.md` for the method in full.",
        "",
        "---",
        "",
        "*Generated by `scripts/retrieval-bench/datapoint.py`. This branch is",
        "regenerated on publish; do not commit to it by hand.*",
    ]

    (staging / "README.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    sys.exit(main())
