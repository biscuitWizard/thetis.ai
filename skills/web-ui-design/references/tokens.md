# Tokens, components and file map

Lookup material for writing actual CSS and markup. The decisions and procedures
are in the parent skill; this is the catalogue.

## Tokens

Every value lives in `gateways/gateway-web/src/ui/theme.css`, the only file
allowed a literal colour. `app.css` refers to these names and nothing else.

### Colour

| Token | Value | Used for |
|---|---|---|
| `--bg` | `#0b0b0f` | Page, and the inset of a `<pre>` |
| `--surface-1` | `#101016` | Sidebar, rail, panel, composer, tool card |
| `--surface-2` | `#16161f` | A card inside a panel, a user bubble, a toast |
| `--surface-3` | `#1d1d28` | Hover state, inline code, a chip, a popover |
| `--hairline` | `#24242f` | Every default border |
| `--hairline-strong` | `#32323f` | Hover border, scrollbar thumb, graph trunk rail |
| `--text` | `#ececf2` | Body text |
| `--text-dim` | `#a3a3b4` | Secondary text, a ghost button's label |
| `--text-faint` | `#6e6e82` | Meta lines, placeholders, counts |
| `--accent` | `#7c9cff` | The one accent. Fills only the send button |
| `--accent-hot` | `#96b0ff` | Accent hover |
| `--accent-deep` | `#4a5fd0` | Defined, unused — available |
| `--accent-wash` | accent at 14% | Active tab, selected row, primary button hover |
| `--accent-edge` | accent at 38% | Focus ring and accent borders |
| `--ok` | `#7fd18f` | Success, an assistant's row head |
| `--warn` | `#e8b673` | Running, dirty, behind trunk, cache breakpoints |
| `--err` | `#f2788f` | Failure, danger, conflict |
| `--ok-wash` `--warn-wash` `--err-wash` | each at 12% | Status pill fills |

Washes are `color-mix(in srgb, var(--x) N%, transparent)`. Compose new ones the
same way rather than adding a hex.

### Type

`--font` is `"Inter", ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif`.
`--mono` is `ui-monospace, "SF Mono", "Cascadia Code", Menlo, Consolas, monospace`.

Mono carries identity and quantity: revisions, branch names, paths, slugs,
costs, token counts, JSON.

| Token | Value | Used for |
|---|---|---|
| `--text-xs` | 11.5px | Meta lines, pills, counts, notes |
| `--text-sm` | 12.5px | Secondary body, card descriptions, buttons |
| `--text-md` | 14px | Body, transcript |
| `--text-lg` | 15.5px | Titles: chat header, panel header |
| `--leading` | 1.65 | Body line height |

Four sizes is the whole scale. A fifth size is almost always the wrong fix for
a hierarchy problem — weight, colour and spacing come first.

**Known gap:** Inter is named but never loaded — no `@font-face`, no link — so
the surface renders in the platform sans. Fixing it means bundling a woff2 as an
asset, not adding a CDN link: the UI is deliberately dependency-free and
self-served.

### Shape, space, elevation, motion

| Group | Tokens |
|---|---|
| Radii | `--r-sm` 7px · `--r-md` 10px · `--r-lg` 14px · `--r-xl` 18px · `--r-pill` 999px |
| Space | `--gap-1` 4 · `--gap-2` 8 · `--gap-3` 12 · `--gap-4` 16 · `--gap-5` 24 · `--gap-6` 32 |
| Shadow | `--shadow-sm` · `--shadow-md` · `--shadow-lg` — floating layers only |
| Motion | `--ease` `cubic-bezier(.2,.7,.3,1)` · `--fast` 110ms · `--med` 180ms, both 0 under `prefers-reduced-motion` · `--sheen` 2600ms, one pass of the working-row sheen (matches `SHEEN_MS` in `views/sessions.js`) |
| Layout | `--sidebar-w` 272px · `--measure` 48rem · `--avatar-lg` 44px · `--avatar-gutter` 56px |

Radius by role: `--r-sm` for inputs and small chips, `--r-md` for cards, buttons
and panels' inner blocks, `--r-lg` for a message bubble, `--r-xl` for the
composer, `--r-pill` for pills and the send button.

### Rail geometry

| Thing | Value |
|---|---|
| Tab strip | 44px, tabs 32×32 with `--r-md` |
| Panel | `min(360px, 42vw)`; `.is-wide` `min(620px, 46vw)` |
| Under 1100px | Panel goes `position: fixed`, floats over the chat, strip stays |
| Under 860px | Sidebar narrows to 210px, composer hint hides |

### Turn avatars

A `.turn-avatar` tile per turn — `--avatar-lg` (44px), `--r-md`, cropped
`object-position: top center` — in a gutter **outside the text column**, not
inside the byline. Squares, not portrait crops: squares line up down the
transcript's edge and keep a face in frame whatever the source ratio.

**There is a gutter on each side: the agent left, the user right**, and each
byline aligns to its own side (`.row.user .row-head { justify-content: flex-end }`).
Which margin holds a picture says whose turn it is before any text is read. It
also fixes a defect of the one-sided version: with padding on the left only, the
whole conversation sat off-centre in the window by half a gutter. Symmetric
padding puts it back (`column_centred` true at every width measured).

**How the gutters are made, and why this way.** `--avatar-gutter` is added to
the *left and right padding of the scroll container*, and the tile is
`position: absolute` into its side (`right: calc(100% + var(--gap-3))` against a
`position: relative` row; the user's overrides with `right: auto` and a matching
`left`). Setting `left` without clearing `right` stretches the tile across the
entire row, since both would apply.
Every centred child of the transcript is `max-width: --measure; margin: 0 auto`,
so shrinking the content box moves all of them together — rows, tool cards,
meta lines, ask forms, the compaction card — with no per-element change. Seven
`max-width: var(--measure)` rules kept working untouched. `.composer-wrap` takes
the same left padding so the composer stays on the text's axis.

Two things this gets right that are easy to get wrong:

- **Padding, not margin.** Margin is slack that vanishes as the window narrows,
  and an avatar positioned into vanished slack lands on the sidebar or the rail.
  Reserved padding shrinks the text column instead — recoverable, and it never
  overlaps. Verified with a rail panel docked at 1440px (`main_w` 764, both
  gutters still 80px, `clear_of_panel` and `clear_of_sidebar` true) and down to
  760px, nothing overflowing at any width.
- **Out of flow, so the byline is untouched.** The tile is a child of the row,
  not of `.row-head`, so the name still starts at the text column's left edge
  (`head_starts_at_row` true) whether or not a face is present.

At ≤860px both tokens shrink together (30px tile, 42px gutter) rather than the
gutter being dropped — dropping it would leave the absolutely-positioned tile
outside the row, over the sidebar. Keep `--avatar-gutter` ≥ avatar + `--gap-3`
for the same reason.

Three rows are built outside the normal `row()` path and each needs the tile
adding by hand: the optimistic "sending" row in `showPending`, and the streaming
row, which must keep its avatar when the final message replaces the streamed
text. All three are worth re-checking after any change here — measured: one tile
each, 12px gap, correct side, `showing: ["img"]` through the delta→final swap.

**Measuring alignment: do not measure the flex container.** `.row-head` is a
full-width flex row, so its rect spans the whole column whatever
`justify-content` does — asserting on it reported *both* `head_at_left` and
`head_at_right` true and would have passed a completely unaligned byline. Select
the text node and measure a `Range` over it instead. And when the head holds more
than a name — the pending row appends a "sending" flag — it is the *last* child
that touches the right edge, so assert on the flag's box and merely that the name
is inboard of it, not that the name itself is flush.

Both roles come from `<template>` elements in `index.html`, cloned per row, so
the markup for a face lives in one place rather than in each renderer.

**The two sources differ, and that asymmetry is the whole design.** The agent's
is `agent.avatar` from config, substituted into its template at serve time, so
it is right on the first paint. Yours lives in the host KV store and arrives on
a `user-avatar` frame *after* rows are on screen — so `draw()` repaints every
`.turn-avatar.is-user` already in the DOM as well as recording the value for
tiles minted later. Without that, an upload appears only on subsequent turns and
a replayed transcript keeps the blank mark. Verified: a row rendered before the
upload picked it up, `user_showing_both: 0`, and a row rendered afterwards came
out already filled.

A role with no face (a system note, a tool row) gets no tile rather than an empty
square: `turnAvatar()` returns null and `el()` skips it.

## Component markup

### Buttons

```html
<button class="ghost-btn">Secondary</button>
<button class="ghost-btn is-primary">Confirm</button>
<button class="ghost-btn is-danger">Delete</button>
<button class="ghost-btn is-wide">Add a model by slug</button>
<button class="icon-btn sm" title="Close" aria-label="Close">…</button>
```

Only `#send` is filled. `is-primary` and `is-danger` tint text and border, and
gain a wash on hover.

### Card, in a panel

```html
<article class="card">            <!-- .is-on for the selected one -->
  <div class="card-head">
    <div class="card-heading">
      <h3 class="card-title">Name</h3>       <!-- .mono for an id or path -->
      <p class="card-meta">identifier</p>
      <p class="card-desc">One line of what it is.</p>
    </div>
    <div class="card-badges"><span class="pill pill-on">universal</span></div>
  </div>
  <details class="card-more"><summary>Details</summary>…</details>
  <div class="card-actions"><button class="ghost-btn">Use</button></div>
</article>
```

Actions go *under* the heading, not beside it: up to four of them squeezed onto
the heading line truncate the identifier, which is the one thing that must stay
readable.

### Section heading

```js
panel.section({ title: "In the picker", count: 4, note: "One sentence on what this group means." })
```

### Segmented control

```html
<div class="segmented">
  <button class="segment is-active">Request</button>
  <button class="segment">Prompt</button>
</div>
```

### Meter

```html
<div style="width:120px;height:4px;background:var(--surface-3);border-radius:var(--r-pill);overflow:hidden">
  <div style="width:31%;height:4px;background:var(--accent);border-radius:var(--r-pill)"></div>
</div>
```

### Disclosure

`<details>` with `summary::-webkit-details-marker { display: none }`, a
hand-rolled chevron from two borders rotated -45°, and 45° when `[open]`. See
`.tool > summary`, `.card-more > summary`, `.ctx-msg > summary`.

## JS helpers

### `lib/dom.js`

- `$(id)` — `getElementById`.
- `el(tag, props, ...children)` — `class`, `dataset`, `on<Event>` handlers,
  `html` (avoid), anything else becomes an attribute. Children may be nodes,
  strings, arrays; `null` and `false` are skipped, which is what makes
  `cond && el(...)` idiomatic.
- `icon(paths, {size, width})` — inline SVG from path strings on a 0 0 20 20 box.
- `clear(node)` — `replaceChildren`, returns the node.
- `onClickOutside(node, fn)` — returns a disposer; deferred a tick so the
  opening click does not immediately close the thing.

### `lib/store.js`

One plain object. `store.set({k: v})` notifies watchers of keys that actually
changed — identity comparison, so mutating an array in place needs
`store.touch("key")`. `store.watch(key, fn)` returns an unsubscribe.

Usage state worth knowing: `turnStats` is one authoritative entry per finished
turn, `liveTurn` is the turn in flight accumulated from each `assistant`
frame's usage (null between turns), and `spendSession` is the finished total —
so anything showing money adds `liveTurn?.cost` to it, or it reads zero for
the whole of a long turn.

Derived helpers live on it: `modeLabel()`, `modelLabel()`,
`baseRevisionLabel()`. `modelLabel()` falls back to the raw slug on purpose — a
conversation can name a model the catalogue no longer offers, and showing
"Default model" there would be a lie about what the turn will use.

### `views/rail.js`

`mountRail(tabs)` · `open(config)` · `close(byUser)` · `isOpen(id)` ·
`activeTab()` · `wantsAutoOpen()` · `show(id)` · `ICONS`.

`open` takes `{id, title, subtitle, blocks}` — or `{items, renderItem, empty}`
for a flat list, where `items: undefined` means "still loading". `head` adds
nodes beside the close button.

### `lib/toast.js`

```js
toast("Updated from trunk — 2 commits merged", { tone: "good" });
toast("Archive failed", { tone: "error" });                    // errors persist
toast("Conversation archived.", { action: { label: "Undo", run } });

popover(anchorEl, {
  message: "Reset the branch to 7bf7a1a?",
  detail: "History is kept; this adds a new commit restoring that state.",
  confirmLabel: "Reset branch",
  danger: true,
  input: { value: "notes.md", mono: true },   // omit for a plain confirm
  onConfirm: (value) => { … },
});
```

Errors stay until dismissed; everything else fades. At most four toasts are
kept — a wall of stale ones is worse than losing the oldest.

### `lib/markdown.js`

`renderMarkdown(text)` returns block nodes: paragraphs, `###` headings, fenced
code with a language strip and a copy button, inline code, bold, italic, http
links, flat lists, blockquotes, rules, GFM pipe tables. Anything else falls
through as text. Streaming stays plain text and is replaced by rendered markdown
when the final `assistant` frame arrives, so a mid-stream reconnect still lands
right.

**Tables** handle two forms. The ordinary multi-line one, and the *collapsed*
one-liner — `| a | b | |---|---| | 1 | 2 |` — which arrives when something has
eaten the newlines, and which models emit often enough to matter. The collapsed
form is genuinely ambiguous: `| |` is both an empty leading cell and a row
boundary. It is resolved by counting cells against the header width
(`chunkRows`), never by guessing. A table needs **two or more** delimiter cells
to be recognised, so a sentence containing a pipe and a dash stays a paragraph.
Wrap in `.md-table-wrap`, which scrolls: a nine-column table is wider than
`--measure` and must not stretch the text column.

### `lib/mermaid.js`

A ```` ```mermaid ```` fence renders as a diagram. **The second approved
exception to "no dependencies"**, after xterm — mermaid 11.17.2 is vendored at
`ui/vendor/mermaid.js`, self-served, so the UI still reaches only its own origin.
The operator chose vendoring over a CDN and the full library over a
flowchart-only subset; do not relitigate either.

Four things hold this together, and none is decorative:

- **It is 3.5 MB, so it loads lazily** — a classic script tag on the first
  mermaid fence, never on the startup path. Verified: a full page load fetches
  only the 9 KB module. It is an IIFE assigning `globalThis.mermaid`, so
  `import` would parse it and define nothing, exactly as with xterm. Build the
  URL relative to `import.meta.url` or it 404s under `/preview/`.
- **Rendered SVG is cached by source.** `renderMarkdown` re-runs on every
  streaming delta, so without this a finished diagram is redrawn dozens of times
  and flickers as the text after it arrives.
- **The fallback is the code block**, with the source and its copy button, plus
  a `--warn`-tinted note. The reader is never worse off than before diagrams
  existed. This is also why the load has a **timeout**: if neither `load` nor
  `error` fires, the promise never settles and the source is stranded behind
  "drawing diagram…" for ever. Only a rejection surfaces the code block.
- **It is the one place model-derived content becomes `innerHTML`**, because
  mermaid's output *is* an SVG string. `securityLevel: "strict"` (its bundled
  DOMPurify), `htmlLabels: false` (SVG `<text>`, no `foreignObject`), and
  parse-before-render keep that honest. Do not remove any of the three.
  Verified with script tags, `onerror` attributes and `javascript:` click
  hrefs in node labels: nothing executed, against a control that did.

Theme comes from `theme.css` via `getComputedStyle`, since mermaid needs
resolved colour strings and cannot read a custom property itself.

## File map

### Guest — `gateways/gateway-web/src/`

| File | Holds |
|---|---|
| `assets.rs` | The served-file table. A new `ui/` file must be added here |
| `handlers.rs` | Guest-side frame dispatch, one function per client action; `tool_group` |
| `render.rs` | Session events → wire frames, one arm per `kind` |
| `ui/index.html` | The shell: three zones and every mount point, by id |
| `ui/theme.css` | Tokens. The only file with literal colour |
| `ui/app.css` | Layout and components |
| `ui/app.js` | Wiring: frame handlers, the rail tab list, `INVALIDATED_BY`, header, actions |
| `ui/lib/` | `dom` · `socket` · `store` · `markdown` · `toast` |
| `ui/views/` | `rail` · `panel` · `transcript` · `composer` · `sessions` · `branch` · `workspace` · `context` · `picker` |

### Host — `crates/thetis/src/`

| File | Holds |
|---|---|
| `web.rs` | The websocket loop, host-frame routing, the loopback origin guard, the 16 MiB cap; `decorate_sessions` and the `activity` push in `write_loop` |
| `activity.rs` | Per-conversation live state folded from worker frames, published on change; the `activity` frame |
| `branch_api.rs` | `branch-*` frames: status, graph, log, merge, update, reset |
| `workspace_api.rs` | `workspace-*` frames, and the one `resolve()` traversal boundary |
| `debug_api.rs` | `debug-request` and `turn-cancel` |
| `host_api.rs` | Host imports; `capture_request` stores each request for the inspector |
| `llm.rs` | `prepare_body` — where the request becomes exactly what the provider sees |

## Wire frames the UI consumes

`catalog` · `sessions` (each row may carry `activity`) · `activity` · `history` · `settings` · `opened` · `accepted` · `event` (with
`kind`: user, delta, assistant, tool-call, tool-result, compacted, nudge, note,
incident, modification, branch-op, turn-started, turn-finished) · `skills` ·
`tools` · `branch-status` · `branch-graph` · `branch-log` · `branch-trunk-log` ·
`branch-result` · `workspace-list` · `workspace-file` · `workspace-result` ·
`debug-request` · `turn-cancel` · `error`.

Frames with no registered handler are dropped silently, so a typo in a
`.on("…")` name shows up as a feature that quietly never works.

Two of these exist for the pending-submission lockout:

- **`accepted`** `{session}` — the guest's answer to a `send`, emitted once
  `host::submit` returns. It carries no message body: the message itself comes
  back through the event stream. This is what unlocks the composer, and it
  arrives *before* the `user` event, not after.
- **`error`** carries **`replying_to`** — the `type` of the client frame that
  failed, set in `web.rs`. Without it the client cannot tell an incidental
  error from the refusal of the thing it is waiting on, and any stray error
  would unlock a composer mid-submission. An older host omits it.
