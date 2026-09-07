---
name = "How the skill system works"
brief = "The four disclosure levels, how retrieval ranks and pins skills for a session, dense and BM25 scoring, parent absorption, and the files on disk."
when_to_use = "Use when you must understand the skill machinery itself: why a skill was or was not retrieved, what the difference is between universal, pinned and searched skills, how ranking and parent absorption work, where skill files live, or how the skills host interface is split from skills-view. For authoring a good skill, use skill-creator instead."
universal = false
tags = ["skills", "retrieval", "embeddings", "bm25", "progressive disclosure", "pinned", "universal", "ranking", "skill_search", "tool-group:selfmod"]
related = ["skill-creator"]
version = 1
---

# How the skill system works

Two views of the same corpus:

- **`skills`** — the agent interface. It can read and write.
- **`skills-view`** — the gateway interface. It can only read. A gateway renders;
  it does not author.

Host code: `crates/thetis/src/skills.rs` parses the files,
`skill_manager.rs` is the service layer, `skill_index.rs` ranks,
`embeddings.rs` gets and caches vectors.

## The four levels

The purpose of the design is that a large corpus costs a **constant** amount of
context. Only briefs are always present. Everything else is pulled when it is
wanted.

| Level | Content | When it loads |
|---|---|---|
| L0 | `brief`, one line | Each system prompt, for `universal = true` skills |
| L1 | `brief`, `when_to_use`, tags, child index | Retrieved for the opening message |
| L2 | The body of `SKILL.md` | `skill_fetch id` |
| L3 | A file under `references/`, `scripts/` or `assets/` | `skill_fetch id file` |

The consequence to remember: **the body does not get you retrieved.** Ranking
sees only `name`, `brief`, `when_to_use` and `tags`.

## Retrieval, once for each session

`retrieve(session_id, query, limit)` ranks the corpus and **pins** the result on
the host side.

The agent calls it one time, in `retrieve_skills_once`, before the first
completion of the first turn. Two reasons:

1. A skill that arrives after the model answered has missed its purpose.
2. Each later turn reads the same pinned set with `pinned()`, so the system
   prompt stays byte-identical and the prompt cache stays warm. If you rank
   again on each turn, you break the cache.

The query is the **first** user message. Later messages are steering inside a
task that the first message already described.

Retrieval is best-effort throughout. If it fails, you work without the extra
cards. That is the state that existed before any of this.

Use **`skill_search`** to look something up in the middle of a conversation. It
ranks but does not pin, so it does not touch the prompt.

## How ranking works

Two paths, one interface:

- **Dense** — cosine similarity over cached 1536-dimension embeddings of the
  card text of each skill. This is the intended path.
- **BM25** — a lexical fallback for when embeddings are not available: no API
  key, an HTTP failure, or a corpus that was never indexed.

The two are **not** fused. On a 35-skill corpus with 28 non-echoing queries,
reciprocal-rank fusion lost to dense alone at every mixing weight that was
tried. Skill cards are short, paraphrased, and semantically near to each other,
which is where lexical overlap misleads. BM25 is insurance, not a contributor.

Then two structural adjustments apply:

- **Parent absorption** — if a parent and its own children both rank, only the
  parent is returned. The card of the parent already indexes the children.
- **Child promotion** — if a child ranks well but its parent does not, the
  parent is pulled in below it, because the body of a child often assumes the
  framing of the parent.

Embeddings are keyed by `(model, dimensions, content_hash)`, where the hash
covers the card text only. Therefore an edit to a body does not pay to embed the
skill again. Prose is cheap to revise.

## Files on disk

The directory is the `paths.skills` setting, which is `skills/` by default.

```text
skills/
  concise.md                    # a bare file is a leaf skill (older form)
  thetis-internals/
    SKILL.md                    # frontmatter plus the body
    references/volatility.md    # an L3 resource
    compaction/
      SKILL.md                  # a nested child
```

Depth is capped at 3. `references`, `scripts` and `assets` are reserved names,
so a nested skill cannot use one.

Files are read on demand, not cached across turns. An edit to a skill takes
effect on the next turn. There is nothing to restart and nothing to register.

## The tools

| Tool | Effect |
|---|---|
| `skill_fetch(id, file?, offset?, limit?)` | L2 body, or an L3 file. Offsets are in characters and never split a character. |
| `skill_search(query, limit?)` | Rank without pinning. |
| `skill_write(id, file?, contents)` | Create or replace the whole file, then lint. Diagnostics come back in the same call. |
| `skill_delete(id, recursive?)` | Refuses a skill that has children unless `recursive`. |
| `skill_lint(id?)` | Lint one skill, or the whole tree. |

An `/` in an id nests the skill under a parent.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A skill is never retrieved | The brief uses your words, not the words of the user | Rewrite the brief and tags. The body cannot help. |
| A child never appears alone | The parent absorbed its rank | Merge it back, or make the trigger of the child clearly different. |
| Only BM25 results appear | No API key, or the provider failed | This is a working fallback, not a fault. Check the key if dense ranking is wanted. |
| A search that echoes your own brief scores well but real queries fail | You tested with author-written words | Test with the words a user would use. An echoing query inflates the apparent score by 20 to 40 points. |
| `skill_write` is refused | The name is reserved, or the depth is over 3 | Read the message. Rename or flatten. |
