---
name = "Web UI design"
brief = "Design and extend the web chat UI: the three-zone shell, the context rail, the design tokens, and the rules that keep new surfaces from becoming modals."
when_to_use = "Use when changing anything the browser shows — adding or restyling a panel, inspector, tab, button, chip, meter or transcript element under gateways/gateway-web/src/ui, picking a colour or a size, or deciding where on screen a new feature belongs. Use it also when a surface feels sloppy, buried or cramped and you need the house rules for fixing it, and when a UI change has to be deployed and verified. Not for the wire protocol's semantics, the agent loop, or tool authoring — though it does say how to add the host-side frame a new panel needs."
universal = false
tags = ["ui", "web ui", "frontend", "css", "design", "design tokens", "panel", "rail", "inspector", "chat interface", "gateway-web", "layout", "restyle", "sloppy ui", "tool-group:selfmod"]
children = "auto"
related = ["thetis-internals", "careful-surgery"]
version = 1
---

# Web UI design

The design language of the chat surface, and the rules that keep it coherent
while it grows. Exact token values and the component catalogue are in
`references/tokens.md` — fetch that when writing actual CSS or markup.

## The shape of the screen

Three zones, left to right. Each has one job, and giving it a second job is how
the surface got sloppy the last time.

| Zone | Width | Job | Never |
|---|---|---|---|
| Sidebar | `--sidebar-w` 272px | Navigate conversations: search, recency groups, archived | Hold a feature. The workspace explorer lived here once and was unfindable |
| Main | `--measure` 48rem, centred | The conversation: transcript, composer | Get replaced by another view |
| Rail | 44px strip + docked panel | Every inspector, as a tab | Cover the conversation |

Inside Main, each turn carries an avatar in a gutter outside the text column —
the agent's on the left, the user's on the right, with each byline aligned to its
own side. The gutters are side padding on the scroll container, so every centred
child moves together and the text keeps its measure. See `references/tokens.md`.

**The one structural rule: an inspector is a rail tab, not a modal.** The rail
docks — the chat reflows beside it and stays readable and typable with anything
open. Before this, six features were full-viewport slide-overs with a scrim, one
at a time, and opening one meant abandoning the conversation you opened it to
understand.

The tabs are Branch, Files, Context, Skills, Tools, Models. Branch is the
resting tab and opens itself once a conversation has a commit graph, so version
state is ever-present rather than behind a click.

### Two directions that were tried and rejected

Do not "improve" the surface back into either of these.

1. **Tabs over the centre stage** (chat and files as peers, the editor filling
   the main zone). This makes the editor a peer of the conversation, and one
   keystroke turns the product into an IDE. The editor matters, but it is
   something you glance at while conversing. Files widens the *rail* instead.
2. **A command palette instead of the rail.** A palette is an accelerator for
   people who already know the surface; it is not information architecture. It
   would leave every panel still hidden. Worth adding *on top of* the rail one
   day, never in place of it.

### Ask which box, before building

A layout request usually names a *neighbour* ("next to the name", "beside the
chat") and not a *box*, and those underdetermine the arrangement. Turn avatars
were built three times: flanking portrait columns, then inline in the byline,
then — correctly — in a gutter outside the text column. Each version satisfied a
plain reading of the words. What settled it was a pair of before/after
screenshots.

So when a request could mean two arrangements, spend the cheap move first: say
which one you are about to build, or ask for a sketch. A wrong layout costs a
full verification cycle, and the user has to look at it to know it is wrong.

## Aesthetic

Dark, near-black neutrals with one periwinkle accent. Depth comes from stepping
up a surface and drawing a 1px hairline, never from a heavy shadow. Shadows are
reserved for things that genuinely float: toasts, popovers, picker menus.

Restraint about the accent is what makes it useful. `--accent` fills exactly two
things — the send button and the active tab's wash. Everything else that wants
emphasis tints its text and its border and leaves the fill alone. A panel full
of filled buttons pulls the eye away from the content it acts on.

Status is a 6–8px dot or an outlined pill, in `--ok` / `--warn` / `--err`. Never
colour a whole row to say "this failed".

## Hard rules

These are not preferences. Breaking one has bitten this codebase.

1. **No literal colour outside `theme.css`.** `app.css` states this at the top
   and means it. Use a token; add one to `theme.css` if none fits. Stale
   fallbacks like `var(--ok, #9ece6a)` are the same bug wearing a hat, and a
   typo'd token name (`var(--sans)` when it is `--font`) fails silently — the
   branch graph's labels inherited the wrong family for weeks.
2. **No `confirm()`, `prompt()` or `alert()`.** Use `popover(anchor, {...})` for
   anything destructive or needing a name, and `toast()` for outcomes. Native
   dialogs cannot say what they are about to destroy, and they cannot be
   anchored to the thing that asked.
3. **Model output is never `innerHTML`.** `lib/markdown.js` builds DOM nodes.
   The `html:` prop on `el()` exists for two hardcoded brand SVGs and nothing
   else.
4. **Dependency-free, no build step.** Plain ES modules, hand-rolled SVG icons.
   A new file under `ui/` must be registered in `gateways/gateway-web/src/assets.rs`
   or it 404s at runtime with the module graph half-loaded.
5. **Hide with the `hidden` attribute — via `setHidden`, not `.hidden =`.**
   `app.css` forces `[hidden] { display: none !important; }` precisely because
   component rules that set their own `display` silently defeat the UA
   stylesheet. But `hidden` is an IDL attribute of `HTMLElement`, and
   `SVGElement` does not inherit it: `svg.hidden = true` sets an ordinary JS
   property and the element stays on screen, with no error. Every avatar in the
   UI is an `<img>` paired with a fallback `<svg>` mark, so this showed up as
   both being visible at once. `setHidden(node, bool)` in `lib/dom.js` goes
   through the attribute and works on either.
6. **Two steps for anything destructive, and name the object.** "Reset the
   branch to 7bf7a1a? — history is kept; this adds a new commit restoring that
   state" beats "Are you sure?". A destructive verb never sits on a bare 5px
   graph node, which is how branch reset used to fire.
7. **Show what the wire already carries, while it is worth seeing.** Per-turn
   cost, tokens, the model and the cache-hit ratio all arrived on every frame
   and were thrown away for months. Before adding a backend field, check
   whether the number is already in hand and merely unrendered. And watch the
   *timing*: accounting hung off `turn-finished` reported `$0.0000 · 0 turns`
   on a conversation forty tool calls into its first turn, because a turn here
   runs for dozens of steps and several dollars before it ends. Each step's
   `assistant` frame carries its own usage — accumulate it as it is spent.
8. **A derived view must declare what invalidates it.** The transcript follows
   the event stream, but a tab showing the filesystem, the tool manifest or the
   last request is drawn once and has no idea the agent just changed what it
   says. Files listed the workspace once and never again; Skills and Tools
   never noticed a skill authored or a tool built. So `app.js` keeps an
   `INVALIDATED_BY` map from tab to the event kinds that stale it, and the open
   tab — only the open one, coalesced — refreshes itself. A new tab whose
   content the agent can change adds an entry there. Refreshing never
   clobbers an open editor: the draft in it is the one thing on the surface the
   host cannot give back.
9. **Every empty state says what to do next.** "No conversations — start one
   with the + button", not "Nothing here".
10. **A silent no-op is a bug.** A control that does nothing without a
    conversation must say why (a toast), not just fail to respond.
11. **An action with a host round trip shows an interstitial and locks its
    input.** The transcript is a pure function of the event log, and the log is
    silent until the host accepts the message — which for a conversation's
    first message is a git branch, a worktree and a worker process, several
    seconds. Clearing the composer into "No messages yet" made a successful
    send look like a no-op. So a submission holds an optimistic row (dimmed,
    "sending"), locks the composer, and keeps the text until acknowledged so a
    failure can hand it back rather than lose it. The same applies to creating
    a conversation.

    **Acknowledgement and echo are two different events, and conflating them
    flashes an empty transcript.** `submit` returns once the session actor has
    the message; the actor appends the `user` event just afterwards, so the
    `accepted` frame usually arrives *first*. Acknowledgement unlocks the
    input; the real event replaces the row. Give the echo a grace timer so a
    broadcast that never arrives cannot strand a placeholder, and never hand
    back text the host has already taken — that is how a message gets sent
    twice.

12. **Broadcasts only reach subscribed tabs.** `web.rs` fans an event out to
    the sockets `watching` that session, so anything derived from another
    conversation — the sidebar's titles and previews — goes stale in a tab that
    was not watching. Re-ask on open rather than assuming a push arrived.

## Component vocabulary

Reach for an existing class before inventing one. Full markup in
`references/tokens.md`.

| Need | Use |
|---|---|
| Secondary action | `.ghost-btn`, plus `.is-primary` / `.is-danger` / `.is-wide` |
| Icon-only action | `.icon-btn`, `.icon-btn.sm` |
| A choice among few views | `.segmented` / `.segment` |
| Selector with a menu | `Picker` (`views/picker.js`) |
| State fact, clickable | `.picker-btn.chat-chip` in the header |
| Label / badge | `.pill` (`.pill-on`, `.pill-warn`, `.pill-error`, `.pill-info`) |
| A row in a panel | `.card` with `.card-head` / `.card-heading` / `.card-badges` |
| Group heading in a panel | `panel.section({title, count, note})` — the note is mandatory in spirit: the grouping *is* the explanation |
| Progress / ratio | a 4px `--surface-3` track with an accent fill |
| Disclosure | `<details>` with the marker suppressed and a rotating chevron |
| Outcome message | `toast(message, {tone, action})` |
| Confirm / rename in place | `popover(anchor, {...})` |

## Adding a rail tab

1. Write the view in `ui/views/<name>.js`. Export a draw function that calls
   `rail.open({id, title, subtitle, blocks})`. Build `blocks` from `panel.*`
   renderers so it matches every other tab.
2. Register the file in `assets.rs`.
3. Add an entry to the `rail.mountRail([...])` list in `app.js`: `id`, `label`,
   `hint` (the tooltip — say what the tab is *for*), `icon` from `rail.ICONS`,
   `wide: true` if it needs the 620px panel, and `activate`.
4. Draw immediately from what the store already holds, then request a refresh.
   A tab that shows "Loading…" when it already had the answer feels broken.
5. If the tab's content is per-conversation, add a case to `refreshOpenTab()` so
   switching conversations follows.
6. If the agent can change what it shows, add an `INVALIDATED_BY` entry naming
   the event kinds that stale it (rule 8).
7. Guard the empty case with a toast, not silence (rule 10).

## Adding a host-side frame

A panel needing data the gateway guest cannot reach — the store, git, the
filesystem, the worker fleet — gets a host-side frame. Follow `debug_api.rs`:

1. A module with `handles(frame_type) -> bool` and
   `handle(&grip, &frame) -> Vec<String>`.
2. Register the prefix in `web.rs`'s connection loop, beside `branch_api` and
   `workspace_api`.
3. Read durable state from the store where you can. Prefer it to asking a
   worker: workers are the shortest-lived thing in the system — reaped when
   idle, restarted onto kernels they built — so a capture held only in worker
   memory is empty exactly when someone goes looking. This is why the request
   inspector persists to the KV store and the gateway reads it back, which also
   makes it answer for stopped and archived conversations.
4. Never spawn a worker as a side effect of opening a panel. Use
   `router.live_peer(session)` and degrade gracefully when there is none.
5. Cap the reply. The websocket ceiling is 16 MiB.
6. Add the client handler in `app.js` with `.on("<frame>", ...)`, and swallow
   `unknown frame type: <prefix>` in the `error` handler so an older host stays
   quiet instead of shouting.

## Deploying and verifying

**The gateway serves the UI built from committed trunk**, out of the build
cache — the log line is `serving trunk's UI from the build cache`. Uncommitted
edits under `ui/` are invisible however many times you restart. So: commit, then
restart, then check. This has wasted a debugging session more than once.

A conversation on its own branch has *not* committed to trunk, so its new
`ui/` files 404 on the live port however green the build is. To see a change
before merging — or when the playwright MCP tools are unavailable — fetch
`web-ui-design/verifying-on-a-branch`: it runs the branch's own gateway on a
spare port and drives headless Chrome over CDP.

Verify in the real browser through the playwright MCP tools, not by reasoning
about the diff:

1. `curl -s http://127.0.0.1:7777/views/<file>.js | grep <a new string>` to
   prove the guest actually rebuilt.
2. Open the page, walk the changed surface, and read the console — zero errors
   is the bar.
3. Exercise the real interaction, including one live turn when the change
   touches the transcript, the composer or usage numbers.

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| A UI edit does nothing after restart | Guest built from committed trunk | Commit, then restart |
| Module 404s, page half-dead | New `ui/` file not in `assets.rs` | Register it |
| An element will not hide | A component rule sets `display` | Use the `hidden` attribute |
| An `<svg>` will not hide, no error | `.hidden = true` on an SVGElement sets a dead JS property | `setHidden(node, true)` from `lib/dom.js` |
| A flex row's children spill past their column | A flex item will not shrink below its content | `min-width: 0` on the one that should give way |
| Something needs to sit beside the centred column | Adding a sibling column reflows everything | Pad the scroll container and position into the padding; every `margin: 0 auto` child follows |
| An alignment assertion passes both ways at once | Measuring a full-width flex container, not its content | Measure a `Range` over the text node |
| An absolutely-positioned element spans its whole row | `left` set without clearing the inherited `right` | `right: auto` alongside the new `left` |
| Colour looks off in one place only | Literal hex or a typo'd token | Token from `theme.css` |
| A panel covers the chat | Built as a modal instead of a rail tab | `rail.open` |
| Handler throws on the second call | Two paths both tearing down (Enter *and* blur) | Make teardown idempotent |
| Header items overlap at narrow widths | Flex children with no `min-width` or `overflow` | Clip the wrap, give the shrinking child a minimum |
| A number the user asked for is missing | It is on the wire and dropped | Render it; check `render.rs` before adding a field |
| A panel sits stale while the agent works | Nothing invalidates it | Add the tab to `INVALIDATED_BY` |
| Totals read zero during a long turn | Accounting hangs off `turn-finished` | Accumulate each `assistant` frame's usage |
| A sidebar row keeps an old title | The tab was not subscribed to that session | Re-ask for the list on open |
