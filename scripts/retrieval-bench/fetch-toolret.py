#!/usr/bin/env python3
"""Turn the public ToolRet benchmark into a corpus our real ranker can rank.

Why an external dataset at all: our hand-written gold set is 61 skills and 36
queries. At that size a two-point change in nDCG is indistinguishable from which
queries somebody happened to write, so it cannot answer "is BM25 reranking worth
what it costs". ToolRet is 44,453 tool documents and 7,961 labelled queries, which
has the statistical power to separate mechanisms rather than just rank them.

    ToolRet: A Benchmark for Evaluating Tool Retrieval (ACL 2025 Findings,
    arXiv 2503.01763), mangopy/tool-retrieval-benchmark, Apache-2.0.

What this produces, written as JSON to --out:

    tools    [{id, group, text}]        the corpus, one entry per tool document
    queries  [{id, query, subset, relevant{id: gain}}]
    groups   [{id, brief, tags, members}]   synthesised, see below

Two honest caveats, because they bound what the numbers mean:

1. **The group buckets are invented.** ToolRet ships no notion of a coarse tool
   group; our router has 16. We synthesise buckets from each tool's own subset
   and documentation, so ToolRet-derived *group routing* numbers measure our
   routing machinery against a plausible taxonomy, not against ground truth.
   The per-tool *ranking* numbers, by contrast, use the dataset's own relevance
   labels and are ground truth.

2. **Tool docs are not skill cards.** A skill card is a couple of hand-written
   sentences; a ToolRet document is a JSON API schema. We compress each document
   to a name/brief/when_to_use shape so the same `index_text()` applies, but a
   ranker tuned for prose may score differently on schemas. That is a property of
   the corpus, and why we keep our own gold set as well rather than replacing it.

The HF rows API is used rather than parquet because pyarrow is not installed here
and the JSON endpoint needs nothing. It pages at 100 rows, so a large pull is
many requests; everything is cached under --cache and re-runs are free.
"""

import argparse
import hashlib
import json
import os
import random
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ROWS = "https://datasets-server.huggingface.co/rows"
INFO = "https://datasets-server.huggingface.co/info"
SPLITS = "https://datasets-server.huggingface.co/splits"
PAGE = 100

# Bucket a tool by what its subset is fundamentally about. Ordered: the first
# match wins, so put the specific before the general. These mirror the *kind* of
# distinction our own table draws (a coarse capability area), not its exact ids,
# because ToolRet's domains do not overlap ours.
SUBSET_GROUPS = [
    ("code", ("code", "github", "git", "repl", "program", "script")),
    ("data", ("sql", "database", "table", "spreadsheet", "csv", "pandas")),
    ("web", ("web", "browser", "url", "http", "search", "google", "bing")),
    ("media", ("image", "video", "audio", "music", "photo", "vision", "speech")),
    ("files", ("file", "path", "directory", "folder", "document", "pdf")),
    ("comms", ("email", "mail", "message", "chat", "slack", "sms", "calendar")),
    ("finance", ("finance", "stock", "crypto", "payment", "bank", "price")),
    ("travel", ("travel", "flight", "hotel", "map", "location", "weather")),
    ("science", ("math", "science", "physics", "chem", "arxiv", "paper")),
    ("commerce", ("shop", "product", "order", "cart", "ecommerce", "amazon")),
    ("social", ("social", "twitter", "reddit", "instagram", "tiktok", "youtube")),
    ("system", ("system", "os", "shell", "terminal", "process", "docker")),
]


def http_json(url, cache_dir, tries=8):
    """GET with a disk cache and backoff. HF rate-limits and occasionally 500s.

    A 10k-document pull is ~100 requests and reliably trips HF's rate limiter, so
    429 is an expected condition rather than an error: it is retried with long,
    honest waits (and `Retry-After` when offered) instead of being surfaced. Every
    successful page is cached before the next is attempted, so an interrupted run
    resumes for free rather than starting the pull again.
    """
    key = hashlib.sha256(url.encode()).hexdigest()[:24]
    path = os.path.join(cache_dir, key + ".json")
    if os.path.exists(path):
        try:
            with open(path) as fh:
                return json.load(fh)
        except (OSError, ValueError):
            pass  # a truncated cache entry should not be fatal

    last = None
    for attempt in range(tries):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "thetis-retrieval-bench"})
            with urllib.request.urlopen(req, timeout=60) as r:
                data = json.load(r)
            os.makedirs(cache_dir, exist_ok=True)
            tmp = path + ".tmp"
            with open(tmp, "w") as fh:
                json.dump(data, fh)
            os.replace(tmp, path)  # atomic, so a killed run leaves no half file
            return data
        except urllib.error.HTTPError as e:
            last = e
            if e.code == 429 and attempt < tries - 1:
                # Prefer the server's own advice; otherwise back off hard, since
                # hammering a rate limiter is what got us throttled.
                wait = 0
                try:
                    wait = int(e.headers.get("Retry-After", 0) or 0)
                except (TypeError, ValueError):
                    wait = 0
                wait = max(wait, min(60, 5 * (attempt + 1)))
                print(f"    rate-limited, waiting {wait}s", file=sys.stderr)
                time.sleep(wait)
                continue
            if attempt < tries - 1:
                time.sleep(2 ** attempt)
        except (urllib.error.URLError, ValueError, TimeoutError) as e:
            last = e
            if attempt < tries - 1:
                time.sleep(2 ** attempt)

    # Returning None lets a partial pull still produce a usable corpus: better a
    # slightly smaller distractor set than no benchmark at all. Callers treat a
    # None page as end-of-data.
    print(f"    giving up on one page after {tries} tries: {last}", file=sys.stderr)
    return None


def page_rows(dataset, config, split, want, cache_dir, label=""):
    """Pull up to `want` rows, following the API's 100-row paging."""
    out = []
    offset = 0
    while len(out) < want:
        n = min(PAGE, want - len(out))
        url = (
            f"{ROWS}?dataset={urllib.parse.quote(dataset, safe='')}"
            f"&config={urllib.parse.quote(config, safe='')}"
            f"&split={urllib.parse.quote(split, safe='')}"
            f"&offset={offset}&length={n}"
        )
        payload = http_json(url, cache_dir)
        if payload is None:
            break  # a page we could not get; keep what we have
        rows = payload.get("rows", [])
        if not rows:
            break
        out.extend(r["row"] for r in rows)
        offset += len(rows)
        total = payload.get("num_rows_total", 0)
        if offset >= total:
            break
        if label and offset % 1000 == 0:
            print(f"    {label}: {offset}/{min(want, total)}", file=sys.stderr)
    return out


def split_names(dataset, cache_dir):
    payload = http_json(f"{SPLITS}?dataset={urllib.parse.quote(dataset, safe='')}", cache_dir)
    if payload is None:
        raise SystemExit(f"cannot list splits for {dataset}; check network access")
    return [(s["config"], s["split"]) for s in payload.get("splits", [])]


def num_examples(dataset, config, split, cache_dir):
    url = f"{INFO}?dataset={urllib.parse.quote(dataset, safe='')}&config={urllib.parse.quote(config, safe='')}"
    payload = http_json(url, cache_dir)
    if payload is None:
        return 0
    info = payload.get("dataset_info", {})
    return info.get("splits", {}).get(split, {}).get("num_examples", 0)


# Most ToolRet subset names are the *dataset* they came from and carry no domain
# meaning: `apibank` is not about banking, `toolbench` is not a workbench. Naive
# substring matching put every apibank tool in `finance` via "bank", which is a
# fabricated signal. Strip these tokens before looking for a domain.
DATASET_NOISE = re.compile(
    r"\b(api|tool|tools|bench|benchmark|gen|ace|alpaca|emu|eyes|lens|ink|sam|"
    r"gorilla|craft|autotools|taskbench|gpt4tools|ultratool|t-eval|eval|"
    r"be-honest|honest|appbench|apibank|toolbench)\b|"
    r"(apibank|toolbench|ultratool|gpt4tools|autotools|taskbench|toolalpaca|"
    r"toolace|toolemu|tooleyes|toollens|toolink|appbench|apigen)",
    re.I,
)


def group_of(subset, text):
    """Assign a synthesised bucket.

    The subset name is checked first but only after dataset-name noise is
    stripped, because a few subsets genuinely encode a domain
    (`autotools-weather`, `taskbench-multimedia`) while most just name their
    source corpus. Whatever survives stripping is real signal; if nothing does,
    fall back to keyword frequency in the tool's own documentation.
    """
    hay = DATASET_NOISE.sub(" ", subset.lower())
    for gid, keys in SUBSET_GROUPS:
        if any(k in hay for k in keys):
            return gid

    hay = text[:600].lower()
    best, score = None, 0
    for gid, keys in SUBSET_GROUPS:
        n = sum(hay.count(k) for k in keys)
        if n > score:
            best, score = gid, n
    return best or "misc"


def flatten_doc(doc):
    """A ToolRet document is a JSON API schema, sometimes double-encoded. Reduce
    it to the fields a card is made of: what it is called, what it does.

    Returns (name, description, parameter names)."""
    if isinstance(doc, str):
        try:
            doc = json.loads(doc)
        except ValueError:
            return "", doc[:400], []
    if not isinstance(doc, dict):
        return "", str(doc)[:400], []

    name = ""
    for k in ("name", "tool_name", "api_name", "function", "title"):
        v = doc.get(k)
        if isinstance(v, str) and v:
            name = v
            break

    desc = ""
    for k in ("description", "doc_description", "desc", "summary", "documentation"):
        v = doc.get(k)
        if isinstance(v, str) and v:
            desc = v
            break

    params = []
    for k in ("parameters", "doc_arguments", "arguments", "args", "input"):
        v = doc.get(k)
        if isinstance(v, dict):
            props = v.get("properties") if isinstance(v.get("properties"), dict) else v
            if isinstance(props, dict):
                params = [p for p in props.keys() if isinstance(p, str)][:12]
            break

    if not desc:
        # Fall back to any nested description, then to the raw JSON.
        m = re.search(r'"description"\s*:\s*"([^"]{4,400})"', json.dumps(doc))
        desc = m.group(1) if m else json.dumps(doc)[:400]
    return name, desc, params


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", default="corpus/toolret.json")
    ap.add_argument("--cache", default=os.environ.get(
        "RETRIEVAL_BENCH_WORK", "/opt/thetis/workspace/zero-retrieval-bench") + "/hf-cache")
    ap.add_argument("--tools", type=int, default=10000,
                    help="tool documents to gather (0 = every one)")
    ap.add_argument("--queries", type=int, default=2000,
                    help="labelled queries to gather (0 = every one)")
    ap.add_argument("--seed", type=int, default=20260830,
                    help="sampling seed; fixed so the corpus is reproducible")
    args = ap.parse_args()

    os.makedirs(args.cache, exist_ok=True)
    rnd = random.Random(args.seed)

    # ---- queries first ------------------------------------------------------
    # The labels carry each relevant tool's `doc` inline, so queries also supply
    # guaranteed-present corpus entries. Gathering them first means every query
    # is answerable even if sampling misses its tools in the corpus pull.
    qsplits = split_names("mangopy/ToolRet-Queries", args.cache)
    print(f"ToolRet-Queries: {len(qsplits)} subsets", file=sys.stderr)

    per = max(1, args.queries // max(1, len(qsplits))) if args.queries else 10 ** 9
    queries, from_labels = [], {}
    for config, split in sorted(qsplits):
        rows = page_rows("mangopy/ToolRet-Queries", config, split, per, args.cache)
        for r in rows:
            labels = r.get("labels")
            if isinstance(labels, str):
                try:
                    labels = json.loads(labels)
                except ValueError:
                    continue
            if not isinstance(labels, list) or not labels:
                continue

            rel = {}
            for lab in labels:
                if not isinstance(lab, dict):
                    continue
                tid = lab.get("id")
                if not tid:
                    continue
                try:
                    gain = float(lab.get("relevance", 1) or 0)
                except (TypeError, ValueError):
                    gain = 1.0
                if gain <= 0:
                    continue
                rel[tid] = gain
                if tid not in from_labels and lab.get("doc") is not None:
                    from_labels[tid] = (config, lab["doc"])
            if not rel:
                continue

            q = (r.get("query") or "").strip()
            if not q:
                continue
            queries.append({
                "id": r.get("id") or f"{config}_{len(queries)}",
                "query": q[:2000],  # matches skills.max_query_chars
                "subset": config,
                "instruction": (r.get("instruction") or "")[:500],
                "relevant": rel,
            })
        print(f"  {config}: {len(rows)} rows, {len(queries)} usable so far",
              file=sys.stderr)

    if not queries:
        raise SystemExit("no usable queries; the dataset schema may have changed")

    # ---- the corpus --------------------------------------------------------
    tools, seen = {}, set()
    for tid, (subset, doc) in from_labels.items():
        name, desc, params = flatten_doc(doc)
        tools[tid] = {"id": tid, "subset": subset, "name": name,
                      "desc": desc, "params": params}
        seen.add(tid)
    print(f"{len(tools)} tools recovered from query labels (all guaranteed relevant)",
          file=sys.stderr)

    # Distractors. Retrieval is only hard when the corpus is full of plausible
    # wrong answers, so top up from ToolRet-Tools well past the relevant set.
    tsplits = split_names("mangopy/ToolRet-Tools", args.cache)
    sizes = {(c, s): num_examples("mangopy/ToolRet-Tools", c, s, args.cache)
             for c, s in tsplits}
    total_avail = sum(sizes.values())
    want = (args.tools or total_avail)
    need = max(0, want - len(tools))
    print(f"ToolRet-Tools: {total_avail} available, pulling ~{need} distractors",
          file=sys.stderr)

    if need:
        # Proportional across configs so the mix is not all `web` (37k of 44k).
        for (config, split), size in sorted(sizes.items()):
            share = int(need * size / total_avail) + 1
            rows = page_rows("mangopy/ToolRet-Tools", config, split,
                             min(share, size), args.cache, label=config)
            for r in rows:
                tid = r.get("id")
                if not tid or tid in seen:
                    continue
                name, desc, params = flatten_doc(r.get("documentation"))
                tools[tid] = {"id": tid, "subset": config, "name": name,
                              "desc": desc, "params": params}
                seen.add(tid)
            print(f"  {config}: corpus now {len(tools)}", file=sys.stderr)

    # ---- shape it into cards ----------------------------------------------
    out_tools = []
    for t in tools.values():
        name = t["name"] or t["id"]
        desc = " ".join(str(t["desc"]).split())[:400]
        params = ", ".join(t["params"])
        group = group_of(t["subset"], f'{name} {desc}')
        # `text` mirrors index_text(): name, then brief, then when_to_use.
        # Parameter names go last because they are weak, noisy signal.
        out_tools.append({
            "id": t["id"],
            "group": group,
            "name": name,
            "brief": desc,
            "when_to_use": f"Parameters: {params}." if params else "",
            "subset": t["subset"],
        })

    # Trim queries whose relevant tools all fell outside the corpus. Keeping one
    # would make it unanswerable and silently depress every mean.
    ids = set(tools)
    kept = []
    for q in queries:
        rel = {k: v for k, v in q["relevant"].items() if k in ids}
        if rel:
            q["relevant"] = rel
            kept.append(q)
    dropped = len(queries) - len(kept)
    queries = kept

    if args.queries and len(queries) > args.queries:
        rnd.shuffle(queries)
        queries = queries[:args.queries]
    queries.sort(key=lambda q: q["id"])  # deterministic order in the file

    # Synthesised group table, built from what actually landed.
    by_group = {}
    for t in out_tools:
        by_group.setdefault(t["group"], []).append(t["id"])
    groups = []
    for gid, members in sorted(by_group.items()):
        keys = dict(SUBSET_GROUPS).get(gid, ())
        groups.append({
            "id": gid,
            "brief": f"Tools for {gid}: {', '.join(list(keys)[:6]) or gid}.",
            "tags": list(keys),
            "members": sorted(members),
        })

    payload = {
        "source": "mangopy/ToolRet-Queries + mangopy/ToolRet-Tools (ACL 2025, Apache-2.0)",
        "note": ("Group buckets are synthesised by this script and are NOT dataset "
                 "ground truth; per-tool relevance labels ARE."),
        "seed": args.seed,
        "tools": sorted(out_tools, key=lambda t: t["id"]),
        "queries": queries,
        "groups": groups,
    }

    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)
    with open(args.out, "w") as fh:
        json.dump(payload, fh, separators=(",", ":"))

    size_mb = os.path.getsize(args.out) / 1e6
    print(f"\nwrote {args.out} ({size_mb:.1f} MB)", file=sys.stderr)
    print(f"  tools   {len(out_tools)}", file=sys.stderr)
    print(f"  queries {len(queries)}"
          + (f" ({dropped} dropped as unanswerable)" if dropped else ""), file=sys.stderr)
    print(f"  groups  {len(groups)}: "
          + ", ".join(f"{g['id']}={len(g['members'])}" for g in groups), file=sys.stderr)
    rels = [len(q["relevant"]) for q in queries]
    if rels:
        print(f"  relevant per query: min {min(rels)}, mean "
              f"{sum(rels)/len(rels):.1f}, max {max(rels)}", file=sys.stderr)


if __name__ == "__main__":
    main()
