# retrieval-bench

Measures two things that are otherwise only ever argued about:

- **SkillRet** — given a realistic opening message, does skill retrieval put the
  right card in the prompt?
- **ToolRet** — given the same message, does group routing attach the tools the
  task needs and withhold the rest?

Both run against the **real** shipping code, and both emit one JSON datapoint
per run so a change can be plotted rather than asserted.

```sh
./ablate.sh                   # THE MEASUREMENT: what each mechanism is worth
./run.sh                      # a single datapoint from the working tree
./run.sh --verbose            # per-case detail, worst first
./run.sh --last 20 --out r.jsonl    # backfill commits (secondary, see below)
./plot.py r.jsonl             # chart a series, and print a delta table
./selftest.sh                 # check the harness itself before committing to it
```

## Start here: ablation, not the time series

The headline question is *"does each retrieval mechanism pay for itself?"* — dense
embeddings cost money per query, so they need to earn it. The way to answer that
is **not** a per-commit chart. It is to hold the corpus and queries fixed at one
commit and toggle one mechanism at a time, with a significance test on the
difference.

`ablate` does that. Each arm changes exactly one thing against a baseline, and
every delta carries a paired-bootstrap 95% CI and p-value, so an arm that merely
looks better is labelled `ns` instead of being reported as a win.

```sh
./ablate.sh                       # both corpora, the committed defaults
./ablate.sh --skills-only         # just the 61-card skills corpus (free, fast)
./ablate.sh --toolret-only        # just the 9.5k-doc external corpus
```

### What it found

**On 9,529 external tool docs / 1,634 queries** (ToolRet, ACL 2025, Apache-2.0):

| arm | nDCG@10 | vs dense | verdict |
|---|---|---|---|
| bm25 | 0.2557 | −0.0781 (p<0.001) | dense is **much** better |
| dense | 0.3338 | baseline | |
| fusion w=0.7 | **0.3565** | +0.0227 (p<0.001) | **better than dense alone** |
| fusion w=0.5 | 0.3345 | +0.0007 | ns |
| absorption/promotion/pool arms | 0.3338 | 0.0000 | no-ops on a flat corpus |

So **dense retrieval earns its cost**: +0.078 nDCG over BM25 at n=1,634, far
outside the CI. And a 0.7-dense/0.3-lexical fusion beats dense alone — which
contradicts an in-repo comment claiming fusion loses at every mixing weight. That
comment was measured on ~35 skills and 28 queries; at that size the effect is
invisible, which is exactly the point of a bigger corpus.

**On our own 61 skill cards / 36 queries**, the same sweep detects almost
nothing: bm25 vs dense is −0.0025, **p=0.93**. Not "BM25 is as good as dense" —
*this corpus cannot tell them apart*. 36 queries is too few to resolve a 0.08
effect. That is the concrete argument for benchmarking against thousands of
documents rather than our own gold set.

One arm on the skills corpus does move, and it is uncomfortable:

| arm | nDCG@4 | hit@1 | vs dense |
|---|---|---|---|
| dense | 0.4621 | 0.806 | baseline |
| no-absorption | 0.6761 | 0.667 | +0.2139 (p<0.001) |

Turning **parent absorption off** raises nDCG sharply while *lowering* hit@1.
Absorption is doing what it was designed to do — collapse a parent and its
children into the parent — and the gold set credits the children it hides. This
is a genuine open question about which behaviour is wanted, not a bug to
auto-revert; see "Reading the absorption result" below.

## The per-commit series is secondary, and here is why

Backfilled across all 104 commits of `origin/main`, BM25, pinned corpus:

- **SkillRet is a flat line** — one distinct value across all 104 points.
- **ToolRet moves** — F1 0.935 → 0.965 → 0.994 as the group table grew 14 → 16
  groups; the step at `af5e45c` added the `subagents` group.

Do **not** read that flat line as "the retrieval work achieved nothing". It is an
artifact of how this repo's history is shaped:

- `crates/thetis/src/skill_index.rs` has **one distinct blob across all 783
  commits in every ref**. The ranker source never changed *in git*.
- Both roots (`097d182`, `5bc9bab`) are **parentless squashes**, and `rank()`,
  `dense_scores`, `bm25_scores`, absorption and promotion are all present, fully
  formed, in the root tree.

The mechanism evolution — grouping, tool fetchers, skill fetchers, BM25
reranking — happened *before* this git history begins and was squashed into the
root commit. There is no commit range over which to measure it. A per-commit
chart therefore cannot answer "was adding BM25 worth it"; only ablation can, by
reconstructing the before-state as an arm. That is why `ablate` is the headline
and `--rev` is kept as a secondary tool.

## Reading the absorption result

The `no-absorption` arm gains **+0.214 nDCG (p<0.001)** on the skills corpus,
which reads like "parent absorption is broken, remove it". Before acting on that,
count what the gold set asks for:

```
cases                        : 36
  gold names parent AND child: 27   <- absorption is penalised here by construction
  gold names only root(s)    :  9
  gold names only child(ren) :  0
```

In **27 of 36 cases** the gold set credits both `thetis-internals` *and*
`thetis-internals/turn-lifecycle`. Absorption exists precisely to return the
parent *instead of* the child. So nDCG docks it for doing its job, and the +0.214
is mostly a statement about how I wrote the gold set, not about retrieval quality.

The honest reading is the pair of numbers together:

| arm | nDCG@4 | hit@1 |
|---|---|---|
| dense | 0.4621 | **0.806** |
| no-absorption | **0.6761** | 0.667 |

Absorption **raises hit@1 by 14 points** and lowers nDCG@4. It makes the single
top result more often right, at the cost of returning fewer distinct relevant
cards. Which is better depends on what the prompt does with the results — and
since the parent card indexes its children, a correct parent at rank 1 is
plausibly worth more than a parent and child at ranks 1 and 2.

**So: do not revert absorption on this evidence.** What would settle it is a gold
set that encodes the intended contract — credit the parent alone where the parent
card genuinely suffices, credit the child only where the child body is required.
That is a gold-set change, and it must land as its own commit so the score
movement is attributable to it rather than to code.

This is the failure mode the whole harness is meant to make visible: a metric
that moves for a reason that has nothing to do with the code under test.

**Standing finding, deliberately not fixed:** every ToolRet failure is the `ssh`
group misfiring through its `server` and `host` tags — on "The API server keeps
returning a 500" and on "explain how a host enrolls". Narrowing those tags is
left undone so that the fix has a before/after to point at.

**`run.sh` is the entry point, not `cargo`.** A bare `cargo build` here fails
with "file not found for module `skills`", because `src/lifted/` holds copies of
the orchestrator's source that only exist once `run.sh` has put them there — they
are build artifacts whose content depends on which revision is being measured, so
they are gitignored rather than checked in. To use cargo directly, lift first:

```sh
./run.sh --lexical            # populates the scratch build under $RETRIEVAL_BENCH_WORK
cd /opt/thetis/workspace/zero-retrieval-bench/build
CARGO_TARGET_DIR=../target cargo test --features toolret
```

`cargo test` there runs this crate's metric tests **and** the lifted ranker's own
unit tests, which is a free correctness check on the code being benchmarked.

## The embedding cache will lie to you if you let it

A dense number is only as good as the vectors behind it, and a vector cache is an
excellent place to hide a wrong answer. This bit me for real: a mock embedding
server used to test the plumbing wrote hash-derived nonsense vectors into the
*same* cache file the real endpoint used, keyed only by
`(model, dimensions, sha256(text))`. Nothing looked broken — the cache was
internally coherent, random-pair cosine sat at a believable 0.16 — but every
dense score on the skills corpus was noise. It reported dense at nDCG 0.169
against BM25's 0.460, which I nearly wrote up as "BM25 beats dense".

Two defences now, both cheap:

1. **The cache filename carries the endpoint.**
   `vectors-text-embedding-3-small-1536-openrouter-ai.json` versus
   `...-127-0-0-1-8799.json`. A mock physically cannot write into the real
   file, and a legacy cache with no recorded origin is discarded rather than
   trusted.
2. **Every dense run verifies before it scores.** Eight texts spread across the
   corpus are re-fetched and compared against what the cache holds. Below cosine
   0.99 the run **aborts** rather than reporting. It checks several rather than
   one because partial poisoning is the likely shape of the fault — a cache is
   filled by more than one run, so the first entry being sound proves nothing.

Verified by deliberately poisoning a cache: the run stops with
`embedding cache is not consistent with the endpoint ... cosine -0.0372`.

If you see that error, delete the named cache file and re-run.

## The one thing to know before trusting a number

`skill_index::rank` short-circuits when `corpus.len() <= limit`: it returns the
whole corpus, unranked. A benchmark run that way scores a perfect 1.0 while
measuring nothing at all. `--limit` therefore defaults to **4**, matching the
shipped `skills.retrieve_limit`, and the harness refuses to run if the limit is
not below the corpus size.

## The corpus is pinned, and why that matters more than it sounds

By default the harness ranks against the frozen fixture in **`corpus/v1`**, not
the live `skills/` tree. `--skills DIR` opts out.

This is not tidiness. The same commit scored `hit@1 0.750` on one checkout and
`0.583` in CI, and nothing about the ranker differed — the local tree had 127
skills, CI's had 61. Thetis pushes selectively, so the live tree is a property of
*which checkout you are standing in*, and it also grows over time as skills and
tool groups are added. A chart built on a live corpus mixes three causes of
movement and cannot attribute any of them.

`corpus/v1` holds 61 skills as frontmatter-only `SKILL.md` files. Bodies are
stripped because `Skill::index_text()` is `name + brief + when_to_use + tags` —
the body is never ranked. Stripping is verified, not assumed: the fixture
reproduces CI's numbers to three decimals.

`torchship/**` is excluded: it is intentionally unpushed, so including it would
make the fixture unbuildable from a fresh clone. The 12 gold cases that named
only torchship skills were dropped with it, taking the skillret set from 48 cases
to 36.

Every datapoint records a `corpus` field, and `plot.py` refuses to join two
points across a corpus change or a mode change — the same rule for both, since
either moves the number without the ranker changing.

Re-pin deliberately, in its own commit, so the chart shows where the corpus
moved:

```bash
./pin-corpus.py --from ../../skills --out corpus/v1 --exclude torchship
./pin-corpus.py --from ../../skills --out corpus/v1 --exclude torchship --check  # drift only
```

### Measuring a change to the skills themselves

A pinned corpus deliberately cannot see a brief edit. To measure one, pin a
fixture per side and diff — this is how the skill-linting commit was assessed:

```bash
for c in 764c77d 3310d8f; do
    d=/tmp/corpus-$c && mkdir -p $d
    git archive $c skills | tar -x -C $d
    ./pin-corpus.py --from $d/skills --out $d/pinned --exclude torchship
    ./run.sh --lexical --skills $d/pinned
done
```

That comparison showed the frontmatter normalisation was retrieval-neutral —
nDCG 0.461 → 0.460, hit@1 0.778 unchanged — while shortening the card text from
58 KB to 49 KB. Worth knowing: it was a large diff across ~50 files whose effect
on retrieval was nil.

## Why the source is lifted rather than reimplemented

`run.sh` copies `skills.rs`, `skill_index.rs` and `skill_lint.rs` out of
`crates/thetis/src/` into a scratch build, and `extract.py` lifts the group table
out of `agents/agent-core/src/groups.rs`. Nothing about ranking is restated.

A hand-written mock of a ranker can pass while the real one is broken, which
makes it worse than no benchmark. The lift also costs nothing: those files depend
only on `anyhow`, `toml`, `tracing` and `std` — no wasmtime, no redb, no host —
so this crate builds in seconds where the orchestrator takes minutes. That is
what makes measuring twenty historical revisions practical.

Two things are *not* lifted, and both are deliberate:

- **`embed.rs`** reimplements the embeddings client. The real one is wired to
  `Config` and redb, and pulling either in would drag wasmtime into a crate that
  otherwise builds in seconds. What is restated is a POST body and a cache key,
  not ranking logic.
- **`toolret::attach`** reproduces `route_once`'s decision in about ten lines,
  because the real one reads and writes a pinned session through host imports
  that do not exist outside wasm. Every number it compares still comes from
  lifted code.

## Measuring past revisions

```sh
./run.sh --rev 5bc9bab                       # one commit
./run.sh --rev HEAD~20..HEAD --out r.jsonl
./run.sh --last 20 --out r.jsonl
./run.sh --branch origin/main --rev all --out r.jsonl   # entire history
```

`--branch REF` picks which history to walk (default `HEAD`), and `--rev all`
covers every commit including the root — needed because git rejects the
`<root>^..tip` form. Walking all 104 commits of `origin/main` takes a few minutes
on a shared target dir.

The harness is **injected into a detached worktree** of the old commit, never
checked out with it. So the gold sets, the metrics and the runner stay fixed
across every point in the series, while what varies is the thing under
measurement: the skill corpus, the ranker, the group table, the tag lists.

This is the only arrangement in which the points are comparable. If the gold set
travelled with the checkout, every point would be scored against a different exam
and the series would mean nothing.

Datapoints produced this way carry `backfilled: true`. They are comparable to
each other and to HEAD, but they were not produced by the tree as it stood.

**What a backfilled point does and does not tell you.** It answers "how would
today's questions, against today's pinned corpus, have scored on that revision's
ranking code". It is not what a user experienced at the time — their corpus was
smaller and the gold set did not exist. That is the right trade for attributing a
change to code, and the wrong one for archaeology about past user experience.

The real checkout is never touched — the worktree is created under
`/opt/thetis/workspace/zero-retrieval-bench/`, which matters because other
conversations share this repo.

Reach degrades rather than failing:

| At the revision | Result |
|---|---|
| No group table (`groups.rs` absent) | SkillRet only, noted in the output |
| No `skill_lint.rs` | `body_diagnostics` stubbed; it is not on the ranking path |
| No `skills.rs` or `skill_index.rs` | Skipped, with a reason |

## The gold sets

`gold/skillret.json` and `gold/toolret.json`, both heavily commented in place.
Two rules worth repeating here:

**Queries are in a user's voice, never the author's.** A query that echoes the
brief it is meant to find scores 20–40 points higher than a real one, so a corpus
tested that way looks fine right up until somebody uses it.

**A parent of a gold child scores 1, not 0.** Parent absorption is deliberate —
the ranker collapses a parent and its children into the parent, whose card
indexes the children, and the agent descends on its own. Scoring that as a miss
would measure the design as a defect.

## The metrics, and why these

| Metric | Why it is here |
|---|---|
| nDCG@k | Relevance is graded, and a set metric would flatten the distinction parent absorption exists to create |
| recall@k | Fails differently from nDCG; the pair localises the fault |
| hit@1 | The only one matching what the agent experiences at `retrieve_limit = 4` |
| MRR | Catches a card slipping from rank 1 to rank 3, which barely moves nDCG |
| ToolRet F1 | Harmonic mean of recall and specificity, so "attach everything" scores 0 rather than 1 |
| ToolRet tags-only | The gap to full F1 is what the skill edge adds — check this before editing a tag list |

Dense and lexical numbers are **not comparable** (~0.3 nDCG apart on this
corpus). Every datapoint records its mode, and `plot.py` refuses to join a line
across a mode change.

## Interpreting a drop

1. `./run.sh --verbose` — the worst cases print first, with what was returned
   and what was missed.
2. Check `unknown_gold_ids`. A renamed skill makes every case naming it
   unwinnable, which looks exactly like a ranking regression.
3. Compare `f1` against `f1_tags_only`. Most apparent tag problems are really a
   missing `tool-group:` tag on a skill.
4. Remember the body cannot help. Ranking sees `name`, `brief`, `when_to_use`
   and `tags` only.

## CI

`.github/workflows/retrieval-bench.yml` runs on pushes touching the corpus, the
ranker or the group table; appends a datapoint to the **`bench-results`** orphan
branch; and publishes the chart as an artifact. A pull request gets the numbers
as a comment but records nothing, since the commits it measures may never land.

To backfill from CI, run the workflow manually with `rev_range` (e.g.
`HEAD~20..HEAD`) or `last`.

```sh
git show bench-results:results.jsonl > /tmp/r.jsonl
./scripts/retrieval-bench/plot.py /tmp/r.jsonl
```

The vector cache is keyed on card text, so changing one brief pays for one
embedding rather than the whole corpus. Without a key the run measures the BM25
fallback and labels itself; it does not fail.

## Relationship to `scripts/group-routing-check`

That harness asserts **invariants** and fails the build: a tool in no group, a
group naming a tool that does not exist, `tool_search` becoming losable. This one
reports a **rate** that is expected to sit below 1.0 and be tracked. A regression
here is a number moving, not a broken build. Keep both.
