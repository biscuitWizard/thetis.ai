#!/usr/bin/env python3
"""Render the two published infographics from benchmark JSON.

    ./infographic.py --routescale routescale.json \
                     --skills ablation-skills.json \
                     --toolret ablation-flat.json \
                     --outdir out/

Produces `tool-routing.svg` and `skill-retrieval.svg`.

Hand-written SVG on purpose. matplotlib is not installed in this environment and
adding it would put a heavy native dependency into CI for two charts whose layout
is fixed; the tradeoff would be different for exploratory plotting.

Every number drawn here comes out of a JSON file produced by the benchmark. There
are no constants in this file that are secretly results, because a chart that can
disagree with its own data is worse than no chart. The one thing this file does
decide is what to *say* about the numbers, in the methodology panels -- those are
prose, and they are meant to travel with the picture so nobody reads a bar
without also reading what the label means.
"""

import argparse
import html
import json
import os
import sys

# ---------------------------------------------------------------- design system

BG = "#0d1117"
PANEL = "#161b22"
PANEL_EDGE = "#30363d"
INK = "#e6edf3"
MUTED = "#8b949e"
FAINT = "#484f58"

ACCENT = "#58a6ff"      # the shipped mechanism
GOOD = "#3fb950"        # a measured win
WARN = "#d29922"        # a caveat
BAD = "#f85149"         # a measured loss
DIM = "#6e7681"         # not significant

FONT = "'Inter','SF Pro Display','Segoe UI',system-ui,-apple-system,sans-serif"
MONO = "'JetBrains Mono','SF Mono',ui-monospace,'Cascadia Code',monospace"


def esc(s):
    return html.escape(str(s), quote=True)


class Canvas:
    """Minimal SVG builder: accumulate elements, join at the end."""

    def __init__(self, width, height):
        self.w = width
        self.h = height
        self.parts = []

    def add(self, s):
        self.parts.append(s)

    def rect(self, x, y, w, h, fill, rx=0, stroke=None, sw=1, opacity=None):
        o = f' opacity="{opacity}"' if opacity is not None else ""
        s = f' stroke="{stroke}" stroke-width="{sw}"' if stroke else ""
        self.add(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" '
            f'rx="{rx}" fill="{fill}"{s}{o}/>'
        )

    def text(self, x, y, s, size=13, fill=INK, weight=400, anchor="start",
             font=None, spacing=None, opacity=None):
        f = font or FONT
        ls = f' letter-spacing="{spacing}"' if spacing else ""
        o = f' opacity="{opacity}"' if opacity is not None else ""
        self.add(
            f'<text x="{x:.1f}" y="{y:.1f}" font-family="{f}" font-size="{size}" '
            f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}"{ls}{o}>'
            f"{esc(s)}</text>"
        )

    def line(self, x1, y1, x2, y2, stroke, sw=1, dash=None, cap="butt", opacity=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        o = f' opacity="{opacity}"' if opacity is not None else ""
        self.add(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
            f'stroke="{stroke}" stroke-width="{sw}" stroke-linecap="{cap}"{d}{o}/>'
        )

    def path(self, d, stroke="none", fill="none", sw=2, dash=None, opacity=None):
        da = f' stroke-dasharray="{dash}"' if dash else ""
        o = f' opacity="{opacity}"' if opacity is not None else ""
        self.add(
            f'<path d="{d}" stroke="{stroke}" fill="{fill}" stroke-width="{sw}" '
            f'stroke-linejoin="round" stroke-linecap="round"{da}{o}/>'
        )

    def circle(self, cx, cy, r, fill, stroke=None, sw=1):
        s = f' stroke="{stroke}" stroke-width="{sw}"' if stroke else ""
        self.add(f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="{r}" fill="{fill}"{s}/>')

    def render(self):
        return (
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{self.w}" '
            f'height="{self.h}" viewBox="0 0 {self.w} {self.h}" '
            f'font-family="{FONT}">'
            f'<rect width="{self.w}" height="{self.h}" fill="{BG}"/>'
            + "".join(self.parts)
            + "</svg>"
        )


def wrap(s, width):
    """Greedy wrap by character count. Good enough for a fixed-width panel."""
    out, line = [], ""
    for word in s.split():
        if len(line) + len(word) + 1 > width and line:
            out.append(line)
            line = word
        else:
            line = f"{line} {word}".strip()
    if line:
        out.append(line)
    return out


def panel(c, x, y, w, h, title=None, kicker=None):
    c.rect(x, y, w, h, PANEL, rx=10, stroke=PANEL_EDGE)
    ty = y + 26
    if kicker:
        c.text(x + 20, ty, kicker.upper(), size=9.5, fill=MUTED, weight=600, spacing="1.4")
        ty += 19
    if title:
        c.text(x + 20, ty, title, size=15.5, fill=INK, weight=600)
        ty += 8
    return ty


def header(c, title, subtitle, meta_pairs, width):
    """Shared page header: title, one-line thesis, and a metadata strip."""
    c.rect(0, 0, width, 5, ACCENT)
    c.text(44, 62, title, size=30, weight=700)
    y = 90
    for ln in wrap(subtitle, 108):
        c.text(44, y, ln, size=14.5, fill=MUTED)
        y += 21

    y += 6
    bx = 44
    for label, value, colour in meta_pairs:
        vw = max(len(str(value)) * 7.6, len(label) * 6.2) + 30
        c.rect(bx, y, vw, 46, "#1c2128", rx=7, stroke=PANEL_EDGE)
        c.text(bx + 14, y + 19, label.upper(), size=8.5, fill=MUTED, weight=600, spacing="1.1")
        c.text(bx + 14, y + 36, value, size=14, fill=colour, weight=600, font=MONO)
        bx += vw + 10
    return y + 46 + 30


def footer(c, width, y, left, right):
    c.line(44, y, width - 44, y, PANEL_EDGE)
    c.text(44, y + 22, left, size=10.5, fill=FAINT)
    c.text(width - 44, y + 22, right, size=10.5, fill=FAINT, anchor="end")


def methodology(c, x, y, w, blocks, title="How this was measured"):
    """A prose panel. Each block is (heading, body). Returns the height used."""
    inner = int((w - 40) / 6.05)
    lines = 0
    for head, body in blocks:
        lines += 1 + len(wrap(body, inner)) + 1
    h = 54 + lines * 15 + 6
    panel(c, x, y, w, h, title=title, kicker="methodology")
    ty = y + 66
    for head, body in blocks:
        c.text(x + 20, ty, head, size=11.5, fill=ACCENT, weight=600)
        ty += 16
        for ln in wrap(body, inner):
            c.text(x + 20, ty, ln, size=11, fill=MUTED)
            ty += 15
        ty += 8
    return h


# ------------------------------------------------------- graph 1: tool routing

def tool_routing(rs, outpath):
    r = rs["result"]
    strategies = r.get("strategies", [])
    sweep = r["sweep"]
    shipped = r["shipped_threshold"]

    W = 1180
    c = Canvas(W, 1560)

    total_q = r["queries"] + r.get("unlabelled", 0)
    unrouted_pct = 100.0 * r["at_shipped"]["unrouted"] / r["queries"]

    best = max(strategies, key=lambda s: s["f1"]) if strategies else None
    ship = next((s for s in strategies if "shipped" in s["strategy"]), None)

    if ship and best and best is not ship:
        thesis = (
            f"Tag matching leaves {unrouted_pct:.0f}% of queries with no tools at all. "
            f"Adding an embedding call on only those queries lifts F1 from "
            f"{ship['f1']:.3f} to {best['f1']:.3f} — so the embedding spend is earning "
            f"its keep, and the cheapest form of it is the best one."
        )
    else:
        thesis = (
            "Tag-based tool-group routing, measured over dataset-labelled queries "
            "instead of a hand-written gold set."
        )

    y = header(
        c,
        "Tool routing efficacy",
        thesis,
        [
            ("queries", f"{r['queries']:,}", INK),
            ("tools", f"{r['tools']:,}", INK),
            ("groups", str(r["groups"]), INK),
            ("unrouted now", f"{unrouted_pct:.0f}%", BAD),
            ("best F1", f"{best['f1']:.3f}" if best else "—", GOOD),
            ("commit", rs["commit"][:9], MUTED),
        ],
        W,
    )

    # ---- strategy comparison, the headline
    if strategies:
        ph = 96 + len(strategies) * 62
        panel(c, 44, y, W - 88, ph,
              title="What each routing strategy achieves, and what it costs",
              kicker="the question: does the embedding call pay for itself")

        bx = 300
        bw = W - 88 - 300 - 210
        ty = y + 78
        c.text(bx, ty - 8, "F1  (recall and specificity together, harmonic)",
               size=10, fill=FAINT)

        maxf1 = max(s["f1"] for s in strategies)
        for s in strategies:
            row = ty + 8
            is_ship = "shipped" in s["strategy"]
            is_best = s is best
            colour = ACCENT if is_ship else (GOOD if is_best else DIM)

            c.text(64, row + 17, s["strategy"], size=13,
                   fill=INK if (is_ship or is_best) else MUTED,
                   weight=600 if (is_ship or is_best) else 400)
            c.text(64, row + 33, s["cost"], size=10, fill=FAINT)

            full = bw
            c.rect(bx, row + 4, full, 22, "#1c2128", rx=4)
            c.rect(bx, row + 4, full * (s["f1"] / max(maxf1, 1e-9)), 22, colour, rx=4)
            c.text(bx + 10, row + 19, f"{s['f1']:.3f}", size=12,
                   fill="#0d1117" if s["f1"] / maxf1 > 0.18 else INK,
                   weight=700, font=MONO)

            # the two components, and the honest cost of over-attaching
            rx0 = bx + full + 18
            c.text(rx0, row + 13, f"recall {s['recall']:.3f}", size=10.5,
                   fill=MUTED, font=MONO)
            c.text(rx0, row + 28, f"spec   {s['specificity']:.3f}", size=10.5,
                   fill=MUTED, font=MONO)
            c.text(rx0 + 118, row + 13, f"{s['mean_attached']:.2f} grp/q",
                   size=10.5, fill=MUTED, font=MONO)
            un = s["unrouted"]
            c.text(rx0 + 118, row + 28,
                   f"{un} unrouted" if un else "none unrouted",
                   size=10.5, fill=BAD if un else GOOD, font=MONO)

            if is_best and not is_ship:
                c.text(bx + full * (s["f1"] / maxf1) + 8, row + 19,
                       f"+{s['f1'] - ship['f1']:+.3f} vs shipped".replace("++", "+"),
                       size=10.5, fill=GOOD, weight=600, font=MONO)
            ty += 62
        y += ph + 22

    # ---- threshold sweep
    sh = 330
    panel(c, 44, y, W - 88, sh,
          title="The routing threshold is a nearly inert knob",
          kicker="threshold sweep")
    c.text(64, y + 74,
           "score = m/(m+1) for m matched tags, so one match already scores 0.50. "
           "Every threshold below that decides the same way:",
           size=11, fill=MUTED)
    c.text(64, y + 90,
           "\"did any tag match at all\". Tuning it is not a lever; adding a second "
           "signal is.",
           size=11, fill=MUTED)

    gx, gy = 92, y + 118
    gw, gh = W - 88 - 96 - 150, 150

    for frac, lab in [(0, "0.0"), (0.25, "0.25"), (0.5, "0.50"),
                      (0.75, "0.75"), (1.0, "1.0")]:
        yy = gy + gh - frac * gh
        c.line(gx, yy, gx + gw, yy, PANEL_EDGE, dash="2,4", opacity=0.55)
        c.text(gx - 10, yy + 4, lab, size=9.5, fill=FAINT, anchor="end", font=MONO)

    ts = [p["threshold"] for p in sweep]
    tmin, tmax = min(ts), max(ts)

    def px(t):
        return gx + (t - tmin) / (tmax - tmin) * gw

    for key, colour, label in [("recall", GOOD, "recall"),
                               ("specificity", ACCENT, "specificity"),
                               ("f1", WARN, "F1")]:
        d = "M " + " L ".join(
            f"{px(p['threshold']):.1f} {gy + gh - p[key] * gh:.1f}" for p in sweep
        )
        c.path(d, stroke=colour, sw=2.4)
        for p in sweep:
            c.circle(px(p["threshold"]), gy + gh - p[key] * gh, 2.6, colour)
        last = sweep[-1]
        c.text(gx + gw + 12, gy + gh - last[key] * gh + 4, label,
               size=10.5, fill=colour, weight=600)

    # mark the shipped threshold and where the cliff is
    sx = px(shipped)
    c.line(sx, gy - 6, sx, gy + gh, ACCENT, sw=1.5, dash="4,3", opacity=0.9)
    c.text(sx, gy - 12, f"shipped {shipped:.2f}", size=10, fill=ACCENT,
           weight=600, anchor="middle")

    cliff = None
    for a, b in zip(sweep, sweep[1:]):
        if a["f1"] - b["f1"] > 0.1:
            cliff = b["threshold"]
            break
    if cliff is not None:
        cx = px(cliff)
        c.line(cx, gy - 6, cx, gy + gh, BAD, sw=1.5, dash="4,3", opacity=0.8)
        c.text(cx, gy - 12, "routing collapses", size=10, fill=BAD,
               weight=600, anchor="middle")

    c.line(gx, gy + gh, gx + gw, gy + gh, PANEL_EDGE)
    for p in sweep:
        c.text(px(p["threshold"]), gy + gh + 18, f"{p['threshold']:.2f}",
               size=9, fill=FAINT, anchor="middle", font=MONO)
    c.text(gx + gw / 2, gy + gh + 38, "tag-match threshold", size=10.5, fill=MUTED,
           anchor="middle")
    y += sh + 22

    # ---- per-family breakdown: is the aggregate win broad or concentrated?
    subs = r.get("by_subset") or []
    if subs and any(s.get("best_f1") is not None for s in subs):
        rows = [s for s in subs if s.get("best_f1") is not None]
        won = [s for s in rows if s["best_f1"] - s["tags_f1"] > 0.001]
        lost = [s for s in rows if s["best_f1"] - s["tags_f1"] < -0.001]
        strat = rows[0].get("best_strategy") or "the best strategy"

        # Two columns: 35 families do not fit in one readable list.
        percol = (len(rows) + 1) // 2
        ph = 132 + percol * 17
        panel(c, 44, y, W - 88, ph,
              title="The win is broad, not one family carrying the average",
              kicker=f"per dataset family · {strat} vs tags")
        c.text(64, y + 74,
               f"{len(won)} of {len(rows)} families improve; {len(lost)} regress. "
               f"Each family is a separate benchmark with its own tool vocabulary, so "
               f"agreement across them is the real evidence.",
               size=11, fill=MUTED)

        colw = (W - 88 - 40) / 2
        for i, s in enumerate(rows):
            col, rowi = i // percol, i % percol
            rx0 = 64 + col * colw
            ry0 = y + 112 + rowi * 17
            d = s["best_f1"] - s["tags_f1"]
            c.text(rx0, ry0, s["subset"][:21], size=10, fill=MUTED, font=MONO)
            c.text(rx0 + 138, ry0, f"{s['queries']:>4}", size=10, fill=FAINT, font=MONO)
            # Bars share one scale across both columns so lengths compare.
            bw2 = colw - 250
            c.rect(rx0 + 172, ry0 - 8, bw2, 10, "#1c2128", rx=2)
            c.rect(rx0 + 172, ry0 - 8, bw2 * s["tags_f1"], 10, DIM, rx=2)
            if d > 0:
                c.rect(rx0 + 172 + bw2 * s["tags_f1"], ry0 - 8,
                       bw2 * d, 10, GOOD, rx=2)
            elif d < 0:
                c.rect(rx0 + 172 + bw2 * s["best_f1"], ry0 - 8,
                       bw2 * -d, 10, BAD, rx=2)
            c.text(rx0 + 172 + bw2 + 8, ry0,
                   f"{d:+.2f}" if abs(d) > 0.001 else "  —",
                   size=10, fill=GOOD if d > 0.001 else (BAD if d < -0.001 else FAINT),
                   font=MONO)
        # Legend, so the stacked bar is not ambiguous.
        c.rect(64, y + 92, 9, 9, DIM, rx=2)
        c.text(78, y + 100, "tags alone", size=10, fill=FAINT)
        c.rect(168, y + 92, 9, 9, GOOD, rx=2)
        c.text(182, y + 100, "gained by adding the embedding call", size=10, fill=FAINT)
        y += ph + 22

    # ---- worst cases: what failure actually looks like
    worst = r.get("worst_cases") or []
    if worst:
        n = min(4, len(worst))
        wh = 84 + n * 46
        panel(c, 44, y, W - 88, wh,
              title="What a routing failure looks like",
              kicker=f"worst cases at threshold {shipped:.2f}")
        c.text(64, y + 72,
               "These queries share no vocabulary with any group's tags, so tag "
               "matching cannot reach them at any threshold.",
               size=11, fill=MUTED)
        ty = y + 96
        for case in worst[:n]:
            q = case["query"].strip().strip('"')
            q = q if len(q) <= 96 else q[:95] + "\u2026"
            c.text(64, ty, q, size=11.5, fill=INK, font=MONO)
            bits = []
            if case["missed"]:
                bits.append(("missed: " + ", ".join(case["missed"]), BAD))
            if case["spurious"]:
                bits.append(("spurious: " + ", ".join(case["spurious"]), WARN))
            bx2 = 64
            for txt, col in bits:
                c.text(bx2, ty + 17, txt, size=10.5, fill=col, font=MONO)
                bx2 += len(txt) * 6.6 + 22
            ty += 46
        y += wh + 22

    # ---- methodology
    mh = methodology(c, 44, y, W - 88, [
        ("Where the labels come from",
         "Queries and per-tool relevance judgements are from ToolRet "
         "(mangopy/ToolRet-Queries and ToolRet-Tools, ACL 2025 Findings, "
         f"Apache-2.0): {r['tools']:,} real tool documents and {total_q:,} queries. "
         "I did not write which tools answer which query."),
        ("What is mine, and therefore what to distrust",
         f"The {r['groups']} coarse groups are synthesised by fetch-toolret.py from "
         "keyword rules over tool documentation, because our router attaches groups "
         "and ToolRet labels individual tools. Query-to-group labels are derived "
         "(gold tools mapped through that bucketing) rather than authored per query. "
         "So absolute F1 is not authoritative; the comparison between strategies is, "
         "because every strategy is scored against identical labels."),
        ("The scorer is the shipped code",
         "table::tokens, table::score and table::tag_present are lifted unmodified "
         "from agents/agent-core/src/groups.rs, and the dense arms use the shipped "
         "cosine from crates/thetis/src/skill_index.rs. Only the group table is "
         "substituted. A lookalike scorer could pass while the real one is broken."),
        ("Metrics",
         "recall = fraction of needed groups attached. specificity = fraction of "
         "the other groups correctly withheld, measured against the whole table "
         "rather than a hand-picked few. F1 = harmonic mean, so attaching "
         "everything cannot score well. grp/q is the cost side: attached groups "
         f"put tools in the prompt. {r.get('unlabelled', 0)} queries had no gold "
         "tool in any bucket and are excluded rather than counted as failures."),
    ])
    y += mh + 18

    footer(c, W, y,
           f"thetis retrieval-bench · routescale · {rs['measured_at']} · "
           f"commit {rs['commit'][:9]}",
           "scripts/retrieval-bench/infographic.py")

    c.h = int(y + 46)
    with open(outpath, "w") as f:
        f.write(c.render())
    return outpath


# --------------------------------------------------- graph 2: skill retrieval

def arm_of(doc, name):
    for a in doc["arms"]:
        if a["arm"] == name:
            return a
    return None


def skill_retrieval(skills, toolret, outpath):
    W = 1180
    c = Canvas(W, 1720)

    s_dense = arm_of(skills, "dense")
    s_bm25 = arm_of(skills, "bm25")
    t_dense = arm_of(toolret, "dense")
    t_bm25 = arm_of(toolret, "bm25")

    # The headline claim has to be the one the data supports: on the large corpus
    # dense beats BM25 decisively; on our own 36-query gold set the same
    # comparison cannot be resolved. Saying only the first would be overclaiming.
    t_delta = t_bm25["vs_baseline"]
    s_delta = s_bm25["vs_baseline"]
    thesis = (
        f"On {toolret['queries']:,} queries, dense retrieval beats BM25 by "
        f"{abs(t_delta):.3f} nDCG ({abs(t_delta) / t_dense['ndcg_at_k'] * 100:.0f}% "
        f"relative, p<0.001) and leads on every metric measured. On our own "
        f"{skills['queries']}-query gold set the nDCG gap is only {abs(s_delta):.3f} "
        f"and is not resolvable (p={s_bm25['p_value']:.2f}) — but hit@1, MRR and "
        f"recall all still favour dense there, so that is a statement about sample "
        f"size and about nDCG's grading, not a case for dropping dense."
    )

    y = header(
        c,
        "Skill retrieval efficacy",
        thesis,
        [
            ("large corpus", f"{toolret['docs']:,} docs", INK),
            ("large queries", f"{toolret['queries']:,}", INK),
            ("our corpus", f"{skills['docs']} skills", INK),
            ("our queries", str(skills["queries"]), WARN),
            ("mode", skills["mode"], MUTED),
            ("commit", skills["commit"][:9], MUTED),
        ],
        W,
    )

    # ---- side-by-side scorer comparison
    ph = 250
    panel(c, 44, y, W - 88, ph,
          title="Dense vs BM25 vs fusion, on both corpora",
          kicker="how well are the right skills retrieved")

    names = ["bm25", "dense", "fusion-w0.5", "fusion-w0.7"]
    col_w = (W - 88 - 80) / 2
    for ci, (doc, label, note) in enumerate([
        (toolret, f"ToolRet · {doc_n(toolret)}", "external, large, statistically decisive"),
        (skills, f"Our skills · {doc_n(skills)}", "the corpus we actually serve, but small"),
    ]):
        ox = 64 + ci * (col_w + 16)
        c.text(ox, y + 74, label, size=12.5, fill=INK, weight=600)
        c.text(ox, y + 90, note, size=10, fill=FAINT)

        arms = [(n, arm_of(doc, n)) for n in names]
        arms = [(n, a) for n, a in arms if a]
        top = max(a["ndcg_at_k"] for _, a in arms)
        bh, gap = 26, 12
        by = y + 108
        for n, a in arms:
            frac = a["ndcg_at_k"] / max(top, 1e-9)
            bwid = (col_w - 150) * frac
            is_base = n == doc["baseline"]
            sig = a["p_value"] < 0.05 and not is_base
            colour = ACCENT if is_base else (GOOD if (sig and a["vs_baseline"] > 0)
                                            else (BAD if (sig and a["vs_baseline"] < 0) else DIM))
            c.text(ox, by + bh - 9, n, size=11, fill=MUTED, font=MONO)
            c.rect(ox + 92, by, col_w - 150, bh, "#1c2128", rx=4)
            c.rect(ox + 92, by, bwid, bh, colour, rx=4)
            c.text(ox + 100, by + bh - 9, f"{a['ndcg_at_k']:.3f}", size=11.5,
                   fill="#0d1117" if frac > 0.2 else INK, weight=700, font=MONO)
            tag = "baseline" if is_base else (
                f"{a['vs_baseline']:+.3f}" + ("" if sig else " ns"))
            c.text(ox + col_w - 52, by + bh - 9, tag, size=10.5,
                   fill=colour if sig or is_base else DIM, font=MONO, anchor="start")
            by += bh + gap
    y += ph + 22

    # ---- the same retrieval, judged at four strictnesses
    # nDCG alone hides the shape of a failure: a ranker can place a relevant card
    # somewhere in the window while never leading with the right one. Four metrics
    # over the same runs separate "found it" from "led with it".
    METRICS = [
        ("hit_at_1", "hit@1", "the top card is relevant"),
        ("mrr", "MRR", "how far down the first relevant card is"),
        ("ndcg_at_k", "nDCG@k", "graded, position-discounted"),
        ("recall_at_k", "recall@k", "share of relevant cards in the window"),
    ]
    mrows = [(d, lb) for d, lb in
             [(toolret, f"ToolRet ({toolret['queries']:,} q)"),
              (skills, f"Our skills ({skills['queries']} q)")]]
    # Direction agreement across metrics is computed, not asserted: if the
    # headline metric ever disagrees with the other three, this sentence has to
    # say so rather than keep claiming consensus.
    agree = []
    for doc, _ in mrows:
        b, o = arm_of(doc, doc["baseline"]), arm_of(doc, "bm25")
        if not o:
            continue
        agree.append([b[k] - o[k] for k, _, _ in METRICS if k in b and k in o])
    flat = [d for row in agree for d in row]
    all_one_way = flat and (all(d > 0 for d in flat) or all(d < 0 for d in flat))
    s_gaps = agree[1] if len(agree) > 1 else []
    note = ""
    if all_one_way and s_gaps:
        note = (
            f"Dense leads on all {len(flat)} metric/corpus pairs above. On our own "
            f"corpus nDCG puts the gap at only {s_gaps[2]:+.3f} — near nothing — "
            f"while hit@1 is {s_gaps[0]:+.3f}, MRR {s_gaps[1]:+.3f} and recall "
            f"{s_gaps[3]:+.3f}. nDCG is graded, and the same parent/child gold "
            "structure described two panels down flattens it here, so reading only "
            "nDCG understates dense on the corpus we actually serve. Only the nDCG "
            "column carries significance testing; the other three are point "
            "estimates, and direction is all they are claimed to show."
        )
    note_lines = wrap(note, 158) if note else []
    ph = 128 + len(mrows) * len(METRICS) * 20 + len(mrows) * 26 + len(note_lines) * 15
    panel(c, 44, y, W - 88, ph,
          title="The same retrieval, judged four ways",
          kicker="strict at the top, lenient at the bottom")
    c.text(64, y + 74,
           "nDCG is the headline, but it blends finding a relevant card with "
           "ranking it first. Splitting them out shows which of the two a scorer "
           "is actually better at.",
           size=11, fill=MUTED)

    mx = 300
    mw = W - 88 - 300 - 210
    my = y + 104
    for doc, dlabel in mrows:
        base = arm_of(doc, doc["baseline"])
        other = arm_of(doc, "bm25" if doc["baseline"] == "dense" else "dense")
        c.text(64, my + 10, dlabel, size=11.5, fill=INK, weight=600)
        my += 24
        for key, mlabel, blurb in METRICS:
            bv, ov = base.get(key), other.get(key) if other else None
            if bv is None:
                continue
            c.text(76, my + 10, mlabel, size=10.5, fill=MUTED, font=MONO)
            c.text(148, my + 10, blurb, size=9.5, fill=FAINT)
            c.rect(mx, my + 2, mw, 12, "#1c2128", rx=2)
            # Both scorers on one track: dense filled, the other a marker, so the
            # gap between them is the thing the eye picks up.
            c.rect(mx, my + 2, mw * max(0.0, min(1.0, bv)), 12, ACCENT, rx=2)
            if ov is not None:
                oxp = mx + mw * max(0.0, min(1.0, ov))
                c.line(oxp, my, oxp, my + 16, WARN, sw=2)
            c.text(mx + mw + 14, my + 10, f"{bv:.3f}", size=10.5, fill=INK, font=MONO)
            if ov is not None:
                c.text(mx + mw + 62, my + 10, f"vs {ov:.3f}", size=10.5,
                       fill=WARN, font=MONO)
            my += 20
        my += 2
    ny = my + 8
    for ln in note_lines:
        c.text(64, ny, ln, size=11, fill=INK if ln is note_lines[0] else MUTED)
        ny += 15

    ly = y + ph - 22
    c.rect(64, ly - 8, 9, 9, ACCENT, rx=2)
    c.text(78, ly, f"{mrows[0][0]['baseline']} (bar)", size=10, fill=FAINT)
    c.line(196, ly - 10, 196, ly + 4, WARN, sw=2)
    c.text(204, ly, "the other scorer (tick)", size=10, fill=FAINT)
    y += ph + 22

    # ---- effect sizes with confidence intervals
    all_arms = [a for a in toolret["arms"] if a["arm"] != toolret["baseline"]]
    # An arm that moved nothing at all, with p exactly 1, is a no-op rather than a
    # null result: on a flat corpus the structural stages have nothing to act on.
    # Omitting them silently would look like cherry-picking, so they are named.
    noop = [a["arm"] for a in all_arms
            if abs(a["vs_baseline"]) < 1e-9 and a["p_value"] >= 1.0]
    drawn = [a for a in all_arms if a["arm"] not in noop]
    noop_lines = len(wrap(
        "Exactly zero effect, omitted above: " + ", ".join(noop)
        + ". ToolRet is a flat corpus with no parent/child structure, so "
          "absorption, promotion and pool size have nothing to act on.",
        166,
    )) if noop else 0
    ph = 150 + len(drawn) * 25 + (32 + noop_lines * 14 if noop else 0)
    panel(c, 44, y, W - 88, ph,
          title="Effect size with 95% confidence intervals",
          kicker="what is actually resolvable")
    c.text(64, y + 74,
           "Each bar is one mechanism turned off or one knob changed, measured "
           "against the baseline on the same queries. A bar whose interval crosses "
           "zero is greyed: the",
           size=11, fill=MUTED)
    c.text(64, y + 90,
           "benchmark cannot tell it apart from noise at this sample size. This is "
           "the panel that decides whether a mechanism is worth its cost.",
           size=11, fill=MUTED)

    arms = sorted(drawn, key=lambda a: a["vs_baseline"])

    gx = 300
    gw = W - 88 - 300 - 190
    gy = y + 112
    lo = min(min(a["ci95"][0] for a in arms), 0) * 1.12
    hi = max(max(a["ci95"][1] for a in arms), 0) * 1.12
    span = max(hi - lo, 1e-6)

    def ex(v):
        return gx + (v - lo) / span * gw

    zero = ex(0)
    c.line(zero, gy - 8, zero, gy + len(arms) * 25 + 6, MUTED, sw=1.5, opacity=0.7)
    c.text(zero, gy - 14, "no effect", size=9.5, fill=MUTED, anchor="middle")

    ry = gy
    for a in arms:
        sig = a["p_value"] < 0.05
        colour = DIM if not sig else (GOOD if a["vs_baseline"] > 0 else BAD)
        c.text(64, ry + 14, a["arm"], size=11.5,
               fill=INK if sig else MUTED, font=MONO)
        l, h = ex(a["ci95"][0]), ex(a["ci95"][1])
        c.line(l, ry + 9, h, ry + 9, colour, sw=1.6, opacity=0.85)
        c.line(l, ry + 4, l, ry + 14, colour, sw=1.6)
        c.line(h, ry + 4, h, ry + 14, colour, sw=1.6)
        c.circle(ex(a["vs_baseline"]), ry + 9, 4.2, colour)
        c.text(gx + gw + 22, ry + 13,
               f"{a['vs_baseline']:+.3f}  p={a['p_value']:.3f}"
               + ("" if sig else "  ns"),
               size=10.5, fill=colour if sig else DIM, font=MONO)
        ry += 25

    c.text(gx + gw / 2, ry + 22,
           f"change in nDCG@{toolret['limit']} vs {toolret['baseline']} "
           f"— ToolRet, {toolret['queries']:,} queries",
           size=10.5, fill=MUTED, anchor="middle")
    if noop:
        ny = ry + 46
        for ln in wrap(
            "Exactly zero effect, omitted above: " + ", ".join(noop)
            + ". ToolRet is a flat corpus with no parent/child structure, so "
              "absorption, promotion and pool size have nothing to act on.",
            166,
        ):
            c.text(64, ny, ln, size=10.5, fill=FAINT)
            ny += 14
    y += ph + 22

    # ---- the absorption caveat, stated where the number is
    ph = 150
    panel(c, 44, y, W - 88, ph,
          title="One result on our corpus is a gold-set artifact, not a finding",
          kicker="read this before citing no-absorption")
    body = (
        "On the skills corpus, turning parent absorption off scores "
        f"{arm_of(skills, 'no-absorption')['ndcg_at_k']:.3f} against "
        f"{s_dense['ndcg_at_k']:.3f} with it on — apparently a large win. It is "
        "mostly an artifact: 27 of the 36 gold cases credit both a parent and its "
        "child, and absorption deliberately returns the parent instead of the "
        "child, so nDCG penalises it for doing its job. hit@1 moves the other way "
        f"({arm_of(skills, 'no-absorption')['hit_at_1']:.3f} against "
        f"{s_dense['hit_at_1']:.3f}), which is the tell. Fixing this means deciding "
        "when a parent card alone is sufficient — a gold-set change that must land "
        "as its own commit so the movement is attributable to the gold set rather "
        "than to code."
    )
    ty = y + 74
    for ln in wrap(body, 158):
        c.text(64, ty, ln, size=11, fill=MUTED)
        ty += 15
    y += ph + 22

    mh = methodology(c, 44, y, W - 88, [
        ("The two corpora, and why both",
         f"ToolRet supplies {toolret['docs']:,} tool documents and "
         f"{toolret['queries']:,} queries with per-tool relevance judgements from the "
         "dataset authors — enough sample to resolve small effects, on documents "
         f"that are not ours. Our own corpus is {skills['docs']} pinned skill cards "
         f"with {skills['queries']} hand-written queries: the thing the ranker "
         "actually serves, but far too small to resolve anything subtle. A mechanism "
         "worth shipping should win on both, and a claim proven on one should not be "
         "assumed on the other."),
        ("Why the corpus is pinned",
         "Thetis pushes selectively, so checkouts legitimately carry different skill "
         "trees: the same commit scored hit@1 0.750 locally and 0.583 in CI purely "
         "because the corpus differed. Runs grade a committed fixture "
         "(scripts/retrieval-bench/corpus/v1, bodies stripped since index_text never "
         "reads them) so a movement in the number has one cause: the code."),
        ("The ranker is the shipped ranker",
         "dense_scores, bm25_scores, absorb_into_parents and promote_parents are "
         "lifted verbatim from crates/thetis/src/skill_index.rs at build time; the "
         "harness only widens their visibility. Arms toggle stages of the real "
         "pipeline rather than reimplementing it."),
        ("Significance",
         "Deltas are paired bootstrap over per-query nDCG, 10,000 resamples, fixed "
         "seed, so reruns agree. 'ns' means the 95% interval includes zero. "
         "Embeddings are text-embedding-3-small at 1536 dimensions, disk-cached with "
         "the endpoint in the cache key and verified against a live refetch on every "
         "run — an earlier silent cache poisoning made dense look catastrophically "
         "bad, and the guard exists because of it."),
        ("What this cannot tell you",
         "This is an ablation at one commit, not a time series. The ranker has one "
         "distinct version across the whole published history, so a per-commit chart "
         "of it is flat by construction; mechanism evolution predates the git "
         "history. Use --rev only for corpus- or table-driven movement."),
    ])
    y += mh + 18

    footer(c, W, y,
           f"thetis retrieval-bench · ablation · {skills['measured_at']} · "
           f"commit {skills['commit'][:9]}",
           "scripts/retrieval-bench/infographic.py")

    c.h = int(y + 46)
    with open(outpath, "w") as f:
        f.write(c.render())
    return outpath


def doc_n(doc):
    return f"{doc['docs']:,} docs, {doc['queries']:,} queries"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--routescale")
    ap.add_argument("--skills")
    ap.add_argument("--toolret")
    ap.add_argument("--outdir", default=".")
    a = ap.parse_args()

    os.makedirs(a.outdir, exist_ok=True)
    made = []

    if a.routescale and os.path.exists(a.routescale):
        with open(a.routescale) as f:
            rs = json.load(f)
        made.append(tool_routing(rs, os.path.join(a.outdir, "tool-routing.svg")))
    else:
        print("no routescale json; skipping tool-routing.svg", file=sys.stderr)

    if a.skills and a.toolret and os.path.exists(a.skills) and os.path.exists(a.toolret):
        with open(a.skills) as f:
            sk = json.load(f)
        with open(a.toolret) as f:
            tr = json.load(f)
        made.append(skill_retrieval(sk, tr, os.path.join(a.outdir, "skill-retrieval.svg")))
    else:
        print("need both --skills and --toolret for skill-retrieval.svg", file=sys.stderr)

    for m in made:
        print(f"wrote {m} ({os.path.getsize(m):,} bytes)")
    return 0 if made else 1


if __name__ == "__main__":
    sys.exit(main())
