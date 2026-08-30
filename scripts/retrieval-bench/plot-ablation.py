#!/usr/bin/env python3
"""Render an ablation run as a horizontal bar chart of deltas against baseline.

A time series is the wrong shape for this data. The question is not "did this
move since Tuesday" but "which mechanisms earn their cost", so each arm gets a
bar showing its nDCG delta with the 95% confidence interval drawn on it. Arms
whose interval crosses zero are greyed: they are the ones where the honest answer
is "no difference detected", and the chart should not let a reader mistake a
noisy bar for a real one.

Reads the JSON that `retrieval-bench ablate --json` writes. No matplotlib: this
runs in CI and hand-written SVG has no install step.

    ./plot-ablation.py ablation.json -o ablation.svg
"""

import argparse
import json
import sys

W, ROW, PAD, LEFT = 900, 30, 60, 250


def esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("input")
    ap.add_argument("-o", "--out", default="ablation.svg")
    args = ap.parse_args()

    with open(args.input) as fh:
        d = json.load(fh)

    arms = d.get("arms", [])
    baseline = d.get("baseline", "")
    if not arms:
        raise SystemExit("no arms in the input file")

    others = [a for a in arms if a["arm"] != baseline]
    if not others:
        raise SystemExit("only a baseline arm; nothing to compare")

    # Symmetric x-scale around zero so a negative bar is visually comparable to a
    # positive one of the same size.
    span = 0.0
    for a in others:
        span = max(span, abs(a.get("vs_baseline") or 0.0))
        ci = a.get("ci95")
        if ci:
            span = max(span, abs(ci[0]), abs(ci[1]))
    span = max(span, 0.01) * 1.15

    height = PAD * 2 + ROW * len(others) + 90
    mid = LEFT + (W - LEFT - 40) / 2
    half = (W - LEFT - 40) / 2

    def x_of(v):
        return mid + (v / span) * half

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{height}" '
        f'viewBox="0 0 {W} {height}" font-family="ui-monospace,monospace">',
        f'<rect width="{W}" height="{height}" fill="#fbfbfd"/>',
    ]

    base_ndcg = next((a["ndcg_at_k"] for a in arms if a["arm"] == baseline), 0.0)
    out.append(
        f'<text x="{PAD}" y="30" font-size="15" font-weight="bold" fill="#111">'
        f'Retrieval ablation: nDCG delta vs {esc(baseline)} '
        f'(baseline {base_ndcg:.4f})</text>'
    )
    out.append(
        f'<text x="{PAD}" y="50" font-size="11" fill="#555">'
        f'{d.get("docs", "?")} documents, {d.get("queries", "?")} queries, '
        f'{esc(d.get("tree", "?"))} tree, limit {d.get("limit", "?")}, '
        f'mode {esc(d.get("mode", "?"))} &#183; bars with 95% CI; '
        f'grey = interval includes zero</text>'
    )

    top = 70
    # Zero line: the reference every bar is read against.
    out.append(
        f'<line x1="{mid:.1f}" y1="{top}" x2="{mid:.1f}" y2="{top + ROW * len(others)}" '
        f'stroke="#888" stroke-width="1.5"/>'
    )

    for i, a in enumerate(others):
        y = top + i * ROW
        cy = y + ROW / 2
        delta = a.get("vs_baseline") or 0.0
        ci = a.get("ci95")
        p = a.get("p_value")
        ns = bool(ci) and ci[0] <= 0.0 <= ci[1]

        colour = "#aaa" if ns else ("#1a7f37" if delta > 0 else "#cf222e")

        if i % 2 == 0:
            out.append(f'<rect x="{LEFT - 8}" y="{y}" width="{W - LEFT - 30}" '
                       f'height="{ROW}" fill="#f0f0f4"/>')

        out.append(
            f'<text x="{LEFT - 16}" y="{cy + 4}" font-size="12" text-anchor="end" '
            f'fill="#111">{esc(a["arm"])}</text>'
        )

        bx, bw = min(mid, x_of(delta)), abs(x_of(delta) - mid)
        out.append(f'<rect x="{bx:.1f}" y="{cy - 7:.1f}" width="{max(bw, 1):.1f}" '
                   f'height="14" fill="{colour}" opacity="0.85"/>')

        if ci:
            lo, hi = x_of(ci[0]), x_of(ci[1])
            out.append(f'<line x1="{lo:.1f}" y1="{cy:.1f}" x2="{hi:.1f}" y2="{cy:.1f}" '
                       f'stroke="#333" stroke-width="1"/>')
            for ex in (lo, hi):
                out.append(f'<line x1="{ex:.1f}" y1="{cy - 5:.1f}" x2="{ex:.1f}" '
                           f'y2="{cy + 5:.1f}" stroke="#333" stroke-width="1"/>')

        label = f'{delta:+.4f}'
        if p is not None:
            label += f'  p={p:.3f}'
        if ns:
            label += '  ns'
        anchor_x = x_of(delta) + (8 if delta >= 0 else -8)
        out.append(
            f'<text x="{anchor_x:.1f}" y="{cy + 4:.1f}" font-size="10.5" '
            f'text-anchor="{"start" if delta >= 0 else "end"}" fill="#333">'
            f'{esc(label)}</text>'
        )

    # Axis ticks, so the bar lengths have a readable scale.
    axis_y = top + ROW * len(others) + 6
    for frac in (-1.0, -0.5, 0.0, 0.5, 1.0):
        v = span * frac
        x = x_of(v)
        out.append(f'<line x1="{x:.1f}" y1="{axis_y}" x2="{x:.1f}" y2="{axis_y + 5}" '
                   f'stroke="#888"/>')
        out.append(f'<text x="{x:.1f}" y="{axis_y + 18}" font-size="10" '
                   f'text-anchor="middle" fill="#555">{v:+.3f}</text>')

    caveat = d.get("caveat", "")
    if caveat:
        out.append(f'<text x="{PAD}" y="{height - 12}" font-size="9.5" fill="#777">'
                   f'{esc(caveat[:150])}</text>')

    out.append("</svg>")

    with open(args.out, "w") as fh:
        fh.write("\n".join(out))

    # A terminal summary too, since CI logs are often all anyone reads.
    print(f"baseline {baseline}: nDCG {base_ndcg:.4f}")
    print(f"{'arm':<28} {'delta':>9} {'p':>7}  verdict")
    print("-" * 62)
    for a in others:
        delta = a.get("vs_baseline") or 0.0
        ci = a.get("ci95")
        p = a.get("p_value")
        ns = bool(ci) and ci[0] <= 0.0 <= ci[1]
        verdict = ("no detectable difference" if ns
                   else ("helps" if delta > 0 else "hurts"))
        print(f'{a["arm"]:<28} {delta:>+9.4f} '
              f'{(p if p is not None else float("nan")):>7.3f}  {verdict}')
    print(f"\nwrote {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
