# House style for skill bodies

Every rule here is checked by `skill_lint`. Nothing in this file is taste that
the linter does not also hold you to, so treat a clean lint as the definition of
done and this file as the explanation of why.

## Linking to another skill

A cross-reference is **data**, not prose. Write it as a markdown link with the
`skill:` scheme:

```markdown
The landed-hit pipeline lives in [combat-damage](skill:torchship/npc-and-life/combat-damage).
```

Rules:

- **Always the full id.** `skill:torchship/world-simulation/action`, never
  `skill:action`. Names like `action`, `checks`, `effect`, `area` and `identity`
  recur across topics; a bare name is ambiguous to any reader who does not
  already know which topic was meant — which is exactly the reader following a
  link. A bare name that happens to be unique still earns a warning; one shared
  by two skills is an **error**.
- **A link that matches no skill is an error.** This is the whole point: rename
  or delete a skill and every stale reference surfaces on the next lint.
- **Links inside code are ignored.** A fenced block or an inline `` `span` ``
  showing this syntax is documentation about links, not a link.

Replace prose "See also" lists. This:

```markdown
See also skills: area, effect (temperature effects), emote.
```

becomes this:

```markdown
See also [area](skill:torchship/world-simulation/area),
[status-effect](skill:torchship/world-simulation/status-effect) for temperature,
and [emote](skill:torchship/npc-and-life/emote).
```

A "See also" line with no linked reference on it is a warning.

Use frontmatter `related` for a reference that belongs to the whole skill rather
than to a sentence. Body links are for "go here for the detail of this thing";
`related` is for "these are its neighbours".

## A parent links every child

An umbrella body must link every one of its nested skills. Omitting one is an
**error**: the generated child index on the card lists ids and briefs, but the
body is where a reader decides which child to open, and a leaf missing from the
body is a leaf nobody fetches.

The parent is a dispatch table. One line per child saying when to reach for it:

```markdown
- [combat-flow](skill:torchship/npc-and-life/combat-flow) — round structure,
  initiative, and how a fight starts and ends.
- [combat-damage](skill:torchship/npc-and-life/combat-damage) — the landed-hit
  pipeline: soak, wound severity, bodypart selection.
```

Do not restate a child's content. If the parent's line and the child's body say
the same thing, the split was wrong.

## Headings

- **One `# Title` as the first non-blank line.** An error otherwise. The body is
  pasted into a conversation under no heading of its own, so without an H1 the
  reader cannot see where the skill starts.
- `##` for sections below it. A second `#` is a warning — it usually means two
  skills in one file.
- No `# ` heading that merely repeats the `name` and then immediately repeats the
  brief. Start at the first instruction.

## Emphasis and caveats

No shouty inline labels: `NOTE:`, `RUNTIME NOTE:`, `WARNING:`, `IMPORTANT:`,
`CAUTION:`, `TODO:` all warn. State the caveat as a sentence, or lead with bold
if it genuinely needs to interrupt:

```markdown
Bad:   RUNTIME NOTE: the cache is not invalidated on rename.
Good:  **The cache is not invalidated on rename**, so a renamed object keeps
       its old vision string until the next tick.
```

## Retiring a skill instead of deleting it

A skill documenting a system that no longer exists is still worth keeping — a
reader arriving from old code needs to know what they are looking at. Mark it
rather than deleting it:

```toml
status = "retired"
superseded_by = "torchship/world-simulation/status-effect"
```

The card then renders as retired and names the replacement, and any body linking
to it gets a warning naming that replacement — so a stale cross-reference
surfaces on the next lint instead of quietly misleading someone. A
`superseded_by` matching no skill is an error; `retired` with no `superseded_by`
is a warning, because "this is dead" without "go here instead" leaves the reader
stuck.

One deliberate exception: a **parent** may link its own retired child without a
warning. The parent is required to index every child, so the two rules would
otherwise contradict each other. Say so in the dispatch line:

```markdown
- [effect](skill:torchship/world-simulation/effect) — legacy, retired. Only when
  repairing old code or migrating it to
  [status-effect](skill:torchship/world-simulation/status-effect).
```

## Two patterns worth copying

The best leaves in the corpus do these; adopt them where they apply.

**A verification snippet.** A copy-pasteable command or code block a reader can
run to confirm the thing still works as described. This is the corpus's weakest
area — most skills say what to do and nothing about how to know it worked.

**A "Known gaps" section.** Three or four lines on what is deliberately
unimplemented. Without it a reader cannot tell what they broke from what was
never built, and spends the difference debugging.

## Register

Dense prose with real identifiers, object numbers, file paths and error text.
Not bullet lists of `key: value` — those read as notes-to-self and give a reader
nothing to act on. If a skill is genuinely three facts long, three sentences of
prose beat three bare bullets.
