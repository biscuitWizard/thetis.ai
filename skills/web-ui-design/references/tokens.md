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
| Motion | `--ease` `cubic-bezier(.2,.7,.3,1)` · `--fast` 110ms · `--med` 180ms, both 0 under `prefers-reduced-motion` |
| Layout | `--sidebar-w` 272px · `--measure` 48rem · `--portrait-w` 132px · `--portrait-w-lg` 208px |

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

### The stage: avatars beside the conversation

`.stage` wraps the transcript in a row with a `.stage-avatar` column either
side — the user's portrait left, the agent's right, matching the side each one's
messages are attributed on. The transcript keeps `--measure` and stays centred;
the columns only take slack the window already had.

**Measure with a container query, not a media query.** `.main` is
`container-type: inline-size`, and the thresholds are on `@container stage`.
The space beside the transcript depends on the docked rail panel as well as the
viewport, and a media query cannot see it — so a viewport rule would keep the
portraits and crush the text the moment an inspector opened. Verified: at a
1600px viewport `.main` is 1284px and the portraits show at 208px each; docking
a panel drops `.main` to 924px and they retract to nothing, transcript back to
full width.

Derive each threshold rather than picking a round number:
`--measure` + 2 × `--gap-5` + 2 × (portrait width + `--gap-4`), rounded up.
That is 1116px for the small portrait and 1268px for the large one. A guessed
1150px looked reasonable and meant the portraits never appeared at 1440px,
because `.main` is only 1124px there.

Both columns are laid out whenever either is, so the centre column cannot drift
off-axis when only one side has a picture.

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
links, flat lists, blockquotes, rules. Anything else falls through as text.
Streaming stays plain text and is replaced by rendered markdown when the final
`assistant` frame arrives, so a mid-stream reconnect still lands right.

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
| `web.rs` | The websocket loop, host-frame routing, the loopback origin guard, the 16 MiB cap |
| `branch_api.rs` | `branch-*` frames: status, graph, log, merge, update, reset |
| `workspace_api.rs` | `workspace-*` frames, and the one `resolve()` traversal boundary |
| `debug_api.rs` | `debug-request` and `turn-cancel` |
| `host_api.rs` | Host imports; `capture_request` stores each request for the inspector |
| `llm.rs` | `prepare_body` — where the request becomes exactly what the provider sees |

## Wire frames the UI consumes

`catalog` · `sessions` · `history` · `settings` · `opened` · `accepted` · `event` (with
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
