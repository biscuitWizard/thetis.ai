---
name = "Writing skills"
brief = "Author, nest and lint skills so retrieval finds them and the body earns its context."
when_to_use = "Use when creating a new skill, editing an existing one, deciding whether something should be a skill at all, or diagnosing why a skill never gets retrieved or fetched. Also when splitting a skill that has grown too long."
universal = true
tags = ["skills", "authoring", "retrieval", "meta"]
children = "auto"
version = 1
---

A skill is procedural knowledge that survives the conversation that produced it.
Writing one is cheap; writing one that actually gets used takes attention to the
two moments it can fail — retrieval, and the decision to fetch.

## The four levels

Disclosure is the whole design. Each level costs more context than the last, so
each has to earn its place.

| Level | What it is | When it loads |
|---|---|---|
| L0 | `brief`, one line | Universal skills, every prompt |
| L1 | `brief` + `when_to_use` + tags + child index | Retrieved for the opening message |
| L2 | the body | `skill_fetch` |
| L3 | `references/`, `scripts/`, `assets/` | `skill_fetch` with `file` |

The consequence worth internalising: **the body is not what gets you retrieved.**
Ranking runs over name, brief, `when_to_use` and tags only. A brilliant body
under a vague brief is dead weight — it will never be read.

## Frontmatter

TOML between `---` fences.

```toml
---
name = "Writing skills"
brief = "Author, nest and lint skills so retrieval finds them."
when_to_use = "Use when creating a new skill, editing one, or diagnosing why a skill is never retrieved."
universal = false
tags = ["skills", "authoring"]
children = "auto"
version = 1
---
```

- **`name`** — human-readable, for display.
- **`brief`** — max 200 characters, hard limit. What this skill does, in one
  line. This is the L0 text and the highest-value string in the file.
- **`when_to_use`** — max 1024 characters. The trigger conditions. Say what
  situations it applies to *and* what it does not cover; a boundary is what stops
  it firing on everything adjacent.
- **`universal`** — in every prompt. Capped at 20 across the corpus. Spend these
  on things that apply to almost any task, never on domain specifics.
- **`tags`** — retrieval terms, and the lexical fallback's only real signal.
- **`children`** — `"auto"` adopts nested directories, or list ids explicitly.
- **`version`** — bump when the body changes meaningfully.

Every field, with its exact limits and defaults, is in
`references/frontmatter.md`. Fetch it when writing frontmatter rather than
guessing at a cap.

## Writing a brief that gets retrieved

The brief and `when_to_use` are matched semantically against what the user
actually said. So write them in the user's vocabulary, not yours.

Bad, because nobody phrases a request this way:

```toml
brief = "Leverages the revision subsystem for mutation-safe component lifecycle management."
```

Good, because it echoes how the need arises:

```toml
brief = "Work in small reversible steps when changing your own loop, gateways or tools."
when_to_use = "Use whenever the target of a change is Thetis itself: the agent loop, a gateway, a tool, the WIT contract. Not for ordinary file edits in a user's project."
```

Concretely:
- Lead with the verb. "Work in small steps", "Search the web", "Convert a file".
- Name the nouns a user would name. Real tool names, real file types, real error
  text. Those are the tokens a query will share.
- State the negative boundary in `when_to_use`. It is what prevents a skill
  ranking for every question in its neighbourhood.
- Do not describe the mechanism. "Uses BM25 fallback" helps nobody decide.

## Writing a body worth fetching

The body is read by a model that has already decided to act. It wants a
procedure, not an essay.

- **Imperatives over description.** "Read the file before patching it" beats
  "it is important that files are read before patching".
- **Under ~500 lines.** Past that, split into children or references.
- **Tables and numbered steps** where there is a sequence or a set of cases.
- **Real examples**, ideally showing the wrong version and the right one.
- **No preamble.** No "this skill will teach you". Start at the first instruction.
- **Say what to do when it fails.** A procedure with no failure branch gets
  abandoned at the first error.

## Deciding to nest

Nesting exists for one reason: a body that has outgrown its context budget.
Depth is capped at 3.

Split when either holds:
1. The body is past ~500 lines and has a natural seam.
2. Two halves are used in mutually exclusive situations, so loading both always
   wastes one.

When you split, the parent becomes a dispatch table — it says what each child
covers and when to reach for it, and holds whatever is common. It does not
duplicate their content. A child that is always read together with its parent
should not have been split out.

Do not nest for tidiness. Three related skills at the top level are cheaper to
retrieve than a parent plus three children, because the parent absorbs its
children's rank and you get one result where you wanted three.

## The procedure

1. **Check it does not exist.** `skill_search` for the topic first. Editing an
   existing skill beats adding a near-duplicate that splits retrieval between
   them.
2. **Write the frontmatter first.** Brief and `when_to_use` before any body. If
   you cannot state the trigger in a sentence, the scope is still muddled.
3. **Write the body**, then cut everything that does not change what a reader
   would do.
4. **`skill_write`** it. Lint comes back in the same call.
5. **Test retrieval** with `skill_search`, using a query phrased the way a user
   would phrase it — *not* words copied from your own brief. Author-written
   queries that echo the text inflate apparent quality by 20-40 points and tell
   you nothing. If it does not surface, fix the brief and tags, not the body.
6. **`skill_lint`** before finishing.

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| Never retrieved | Brief in your vocabulary, not the user's | Rewrite with words a request would contain |
| Retrieved, never fetched | Brief describes the topic, not the payoff | Make the brief promise something actionable |
| Fires on unrelated tasks | No negative boundary | Add exclusions to `when_to_use` |
| Fetched and ignored | Body is prose | Convert to numbered steps |
| Child never surfaces alone | Parent absorbs its rank | Merge back, or make the child's trigger genuinely distinct |
