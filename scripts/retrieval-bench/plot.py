#!/usr/bin/env python3
"""Render a JSONL series of benchmark datapoints as a chart and a table.

    ./plot.py results.jsonl                 write results.svg and print a table
    ./plot.py results.jsonl --out chart.svg
    ./plot.py results.jsonl --table         table only

Plain SVG by hand rather than matplotlib: this runs in CI, where a chart is the
one artifact anybody looks at, and a 40MB dependency to draw six polylines is a
poor trade. It also means the output diffs readably.

Dense and lexical runs are drawn as SEPARATE series, never joined. They measure
different code paths and differ by ~0.3 nDCG on this corpus, so a line crossing
between them would read as a dramatic regression where nothing changed but the
availability of an API key.
"""
import json
import sys
from collections import defaultdict

W, H = 900, 380
PAD_L, PAD_R, PAD_T, PAD_B = 60, 170, 30, 70

SERIES = [
    ("skillret", "ndcg_at_k", "SkillRet nDCG@k", "#4f8ef7"),
    ("skillret", "hit_at_1", "SkillRet hit@1", "#7ec8e3"),
    ("skillret", "recall_at_k", "SkillRet recall@k", "#b07ef7"),
    ("toolret", "f1", "ToolRet F1", "#3fb950"),
    ("toolret", "recall", "ToolRet recall", "#8fd694"),
    ("toolret", "f1_tags_only", "ToolRet F1 (tags only)", "#d29922"),
]


def load(path):
    points = []
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        d = json.loads(line)
        if d.get("schema") != 1:
            sys.stderr.write(
                f"skipping a datapoint with schema {d.get('schema')!r}: "
                "this script reads schema 1\n"
            )
            continue
        points.append(d)
    # Chronological by commit date, so the x axis is history rather than the
    # order runs happened to be appended in.
    points.sort(key=lambda d: (d.get("commit_date", ""), d.get("measured_at", "")))
    return points


def value(point, section, key):
    part = point.get(section)
    if not isinstance(part, dict):
        return None
    v = part.get(key)
    return v if isinstance(v, (int, float)) else None


def mode_of(point):
    return "dense" if "dense" in point.get("skillret", {}).get("mode", "") else "lexical"


def svg(points):
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" font-family="ui-sans-serif,system-ui,sans-serif">',
        f'<rect width="{W}" height="{H}" fill="#0d1117"/>',
        f'<text x="{PAD_L}" y="20" fill="#e6edf3" font-size="13">'
        f"Thetis retrieval benchmark &#183; {len(points)} datapoint(s)</text>",
    ]

    plot_w = W - PAD_L - PAD_R
    plot_h = H - PAD_T - PAD_B

    def x_of(i):
        if len(points) == 1:
            return PAD_L + plot_w / 2
        return PAD_L + plot_w * i / (len(points) - 1)

    def y_of(v):
        return PAD_T + plot_h * (1 - v)

    # Gridlines at each 0.2, labelled: without them a reader cannot tell 0.45
    # from 0.75 by eye, which is the whole difference between the paths here.
    for tick in range(6):
        v = tick / 5
        y = y_of(v)
        out.append(
            f'<line x1="{PAD_L}" y1="{y:.1f}" x2="{PAD_L + plot_w}" y2="{y:.1f}" '
            f'stroke="#30363d" stroke-width="1"/>'
        )
        out.append(
            f'<text x="{PAD_L - 8}" y="{y + 4:.1f}" fill="#8b949e" font-size="10" '
            f'text-anchor="end">{v:.1f}</text>'
        )

    # Split each metric into runs of consecutive same-mode points.
    for section, key, label, colour in SERIES:
        segments, current = [], []
        for i, p in enumerate(points):
            v = value(p, section, key)
            m = mode_of(p)
            if v is None:
                if current:
                    segments.append(current)
                    current = []
                continue
            if current and current[-1][2] != m:
                segments.append(current)
                current = []
            current.append((i, v, m))
        if current:
            segments.append(current)

        for seg in segments:
            dash = ' stroke-dasharray="4 3"' if seg[0][2] == "lexical" else ""
            if len(seg) > 1:
                pts = " ".join(f"{x_of(i):.1f},{y_of(v):.1f}" for i, v, _ in seg)
                out.append(
                    f'<polyline points="{pts}" fill="none" stroke="{colour}" '
                    f'stroke-width="2"{dash}/>'
                )
            for i, v, _ in seg:
                out.append(
                    f'<circle cx="{x_of(i):.1f}" cy="{y_of(v):.1f}" r="2.5" '
                    f'fill="{colour}"/>'
                )

    # x labels: short shas, thinned so they do not collide.
    step = max(1, len(points) // 12)
    for i, p in enumerate(points):
        if i % step and i != len(points) - 1:
            continue
        x = x_of(i)
        out.append(
            f'<text x="{x:.1f}" y="{PAD_T + plot_h + 16}" fill="#8b949e" font-size="9" '
            f'text-anchor="end" transform="rotate(-45 {x:.1f} {PAD_T + plot_h + 16})">'
            f'{p.get("commit", "?")[:7]}</text>'
        )

    # Legend.
    for n, (_, _, label, colour) in enumerate(SERIES):
        y = PAD_T + 6 + n * 17
        lx = PAD_L + plot_w + 14
        out.append(
            f'<line x1="{lx}" y1="{y}" x2="{lx + 18}" y2="{y}" stroke="{colour}" '
            f'stroke-width="2"/>'
        )
        out.append(
            f'<text x="{lx + 24}" y="{y + 4}" fill="#c9d1d9" font-size="10">{label}</text>'
        )

    out.append(
        f'<text x="{PAD_L}" y="{H - 12}" fill="#8b949e" font-size="9">'
        "solid = dense &#183; dashed = lexical (BM25 fallback) &#183; "
        "series are never joined across modes</text>"
    )
    out.append("</svg>")
    return "\n".join(out)


def table(points):
    rows = [
        f'{"commit":9} {"date":11} {"mode":8} {"corp":5} {"nDCG":6} {"hit@1":6} '
        f'{"tF1":6} {"tRec":6}  subject'
    ]
    for p in points:
        s = p.get("skillret", {})
        t = p.get("toolret") or {}

        def fmt(v):
            return f"{v:.3f} " if isinstance(v, (int, float)) else "  --   "

        rows.append(
            f'{p.get("commit","?")[:8]:9} {p.get("commit_date","")[:10]:11} '
            f'{mode_of(p):8} {s.get("corpus_size","?"):<5} '
            f'{fmt(s.get("ndcg_at_k"))} {fmt(s.get("hit_at_1"))} '
            f'{fmt(t.get("f1"))} {fmt(t.get("recall"))} {p.get("subject","")[:34]}'
        )

    if len(points) >= 2:
        a, b = points[0], points[-1]
        rows.append("")
        if mode_of(a) != mode_of(b):
            rows.append(
                f"first and last differ in mode ({mode_of(a)} vs {mode_of(b)}); "
                "not comparable"
            )
        else:
            for section, key, label, _ in SERIES:
                va, vb = value(a, section, key), value(b, section, key)
                if va is None or vb is None:
                    continue
                d = vb - va
                mark = "  <-- moved" if abs(d) >= 0.01 else ""
                rows.append(f"  {label:24} {va:.3f} -> {vb:.3f}  ({d:+.3f}){mark}")
    return "\n".join(rows)


def main():
    args = sys.argv[1:]
    if not args:
        sys.stderr.write(__doc__)
        sys.exit(2)

    path = args[0]
    points = load(path)
    if not points:
        sys.stderr.write("no readable datapoints\n")
        sys.exit(1)

    print(table(points))

    if "--table" in args:
        return
    out = args[args.index("--out") + 1] if "--out" in args else (
        path.rsplit(".", 1)[0] + ".svg"
    )
    with open(out, "w") as f:
        f.write(svg(points))
    sys.stderr.write(f"\nwrote {out}\n")


if __name__ == "__main__":
    main()
