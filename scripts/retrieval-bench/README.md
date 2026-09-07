# retrieval-bench

Measures two things that are otherwise only ever argued about:

- **SkillRet** — given a realistic opening message, does skill retrieval put the
  right card in the prompt?
- **ToolRet** — given the same message, does group routing attach the tools the
  task needs and withhold the rest?

Both run against the **real** shipping code, and both emit one JSON datapoint
per run so a change can be plotted rather than asserted.

```sh
./run.sh                      # measure the working tree
./run.sh --verbose            # per-case detail, worst first
./run.sh --lexical            # force the BM25 path
./run.sh --last 20 --out r.jsonl    # backfill the last 20 commits
./plot.py r.jsonl             # chart it, and print a delta table
```

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

## The one thing to know before trusting a number

`skill_index::rank` short-circuits when `corpus.len() <= limit`: it returns the
whole corpus, unranked. A benchmark run that way scores a perfect 1.0 while
measuring nothing at all. `--limit` therefore defaults to **4**, matching the
shipped `skills.retrieve_limit`, and the harness refuses to run if the limit is
not below the corpus size.

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
./run.sh --rev 5bc9bab                  # one commit
./run.sh --rev HEAD~20..HEAD --out r.jsonl
./run.sh --last 20 --out r.jsonl
```

The harness is **injected into a detached worktree** of the old commit, never
checked out with it. So the gold sets, the metrics and the runner stay fixed
across every point in the series, while what varies is the thing under
measurement: the skill corpus, the ranker, the group table, the tag lists.

This is the only arrangement in which the points are comparable. If the gold set
travelled with the checkout, every point would be scored against a different exam
and the series would mean nothing.

Datapoints produced this way carry `backfilled: true`. They are comparable to
each other and to HEAD, but they were not produced by the tree as it stood.

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
