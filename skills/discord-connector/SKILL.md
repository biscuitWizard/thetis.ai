---
name = "Discord connector"
brief = "How the Discord bot connector works, and why its read-only guarantee rests on the session mode."
when_to_use = "Use when changing the Discord connector, adding another messaging surface (Slack, Telegram), or reasoning about what a chat surface is allowed to do. Also read it before exposing any new command over chat, because some commands would break the safety property."
tags = ["discord", "gateway", "security", "modes", "kernel", "tool-group:selfmod", "tool-group:config"]
children = "auto"
version = 3
---

# Discord connector

Source: `crates/thetis/src/discord/` — `mod.rs` (wiring, pairing, streaming),
`api.rs` (gateway socket and REST), `policy.rs` (routing and authorization, as
pure functions), `commands.rs` (the commands themselves and the schema Discord
is told about).

## Why it is in the kernel and not a gateway component

A WebAssembly gateway cannot be a Discord bot. The `gateway` world exports only
`serve-asset`, `on-client-message`, `render-event` and `describe`, and every one
is called *in response* to something arriving. Nothing calls a gateway on a
timer, the host builds a fresh instance per call so no socket can be held open,
and `wasi:http` has no WebSocket upgrade. Discord's gateway is an outbound WSS
connection that must be heartbeated, so it lives in the orchestrator and reaches
the agent through the same `grip.submit` path the browser uses.

## The safety property, and how to not break it

The connector adds **no tool policy of its own**. It stamps every session it
creates with a read-only mode (`discord.mode`, default `chat`), and
`agents/agent-core/src/tools.rs` does the rest, twice over: mutating tools are
withheld from the definitions the model sees, *and* refused again at dispatch.

Two rules follow, and both matter more than they look:

1. **Never expose a way to change the session mode over chat.** The whole
   guarantee is the mode. `/model` changes only which model answers; there is
   deliberately no `/mode`, and the name is caught and refused explicitly.
2. **Never let the connector fall back to a default mode.** `read_only()` in the
   agent ends in `.unwrap_or(false)`, so an *unknown* mode id means full access.
   `spawn()` therefore refuses to start when `discord.mode` is missing or is not
   read-only, and logs why. Verify this by running with `DISCORD_MODE=agent`.

A read-only mode also withholds every hot-loaded tool component unless it
declares the `read-only` capability in its `describe`. That is why the web tools
carry `capabilities: ["http", "read-only"]`; a tool that says nothing is treated
as mutating.

## Slash commands must be registered, or they never arrive

This is worth understanding before touching the command path, because the
failure looks like nothing at all.

The Discord *client* owns the `/` prefix. It intercepts what you type, matches it
against the commands the application has registered, and sends an
`INTERACTION_CREATE` — a different dispatch from `MESSAGE_CREATE`. If nothing
matches, the client refuses to send it and the bot is never told. So a connector
that only reads messages sees no slash commands whatsoever, and `/new` appears to
do nothing while ordinary chat works fine.

Registration is a bulk overwrite: `PUT /applications/{app_id}/commands` with the
full array. It replaces the previous set, so a renamed command does not leave a
stale entry in the picker. Details that matter:

- **The application id is not the bot user id.** They coincide on older apps but
  are separate fields; read `application.id` from READY.
- **Global scope, and it is slow.** A global command can take up to an hour to
  appear in a guild that was already joined; guild scope is instant but only in
  that guild, and registering both scopes shows the command *twice* in the
  picker. Say the propagation delay out loud — otherwise it reads as a bug.
- **Register once per process, not per reconnect.** A reconnect does not clear
  commands, and the endpoint is globally rate-limited.
- **Always name `integration_types` and `contexts`.** This is the opposite of
  what this file said until a `/help` that had never worked was traced to it,
  and it is the whole reason slash commands can be registered yet unusable.
  Omitting them does *not* inherit the application's contexts: Discord stores
  `contexts: null`, which is not the documented "all contexts" default but a
  limbo state the client's picker never offers. The registration returns 200,
  `GET /applications/{app}/commands` lists all of them, the journal logs a
  cheerful count — and typing `/` in a guild shows nothing, so no
  `INTERACTION_CREATE` is ever sent and no error appears anywhere. Discord's own
  tracker carries this as discord-api-docs #7108, #6744 and #7396; the fixes
  applied to newly created commands and never backfilled existing ones.
  Send `"integration_types": [0]` (guild install) and `"contexts": [0, 1]`
  (guild, bot DM). Verified live: Discord echoes both fields back on all eight.
  - **`[0]`, not `[0, 1]`, for `integration_types`** — deliberately. User
    install would let this privileged agent be invoked in any server or DM the
    installing user can reach, outside the channel authorization in `policy.rs`.
  - **Context 2 (private channel) is not requested,** for the same reason, and a
    test pins that.
  - The "Unknown integration" failure that motivated the old advice happens only
    when a command asks for an installation context the *application* does not
    have enabled. Guild install is always available, so naming `[0]` is safe.
  - **The guild-scoped endpoint stores `contexts: null` no matter what you
    send.** Proven live against both guilds. It is moot under the global-only
    design, but do not use guild scope to work around a contexts problem.
  - **A 200 is not evidence of success here.** `api::register_commands`
    therefore inspects the response, warns naming any command that came back
    without contexts, and returns only the count of *usable* ones, so an
    all-limbo registration logs an error instead of "registered 8 commands".
    `commands_without_contexts` is pure and unit-tested on that exact shape.
- **The bot needs the `applications.commands` OAuth scope** in its invite URL. It
  comes free with the `bot` scope, so an ordinary invite already has it, but a
  bot invited with a hand-built URL may not.
- **Answer within three seconds** or the user sees "the application did not
  respond". Reply to `POST /interactions/{id}/{token}/callback`, which is
  authenticated by the token *in the path* and must not carry an Authorization
  header. Command replies use the ephemeral flag (`1 << 6`), since echoing
  configuration into a shared channel is noise for everyone else.

Two invariants keep the two entry points honest:

1. **One implementation.** `commands::run` serves both the interaction path and
   the same words typed literally, which is what a client sends during the
   propagation hour. A test asserts every advertised command is handled, and that
   `/mode` is never advertised.
2. **The same session key.** An interaction is not a message, so it must derive
   the conversation key from parts via `policy::session_key_for`; a test asserts
   both paths agree. Get this wrong and `/new` resets a session nobody is in.

Authorization is re-checked on the interaction path. It never passes through
`decide`, so omitting it would make every command reachable by anyone who can see
the bot.

## INTERACTION_CREATE carries three different things

`INTERACTION_CREATE` is not only slash commands. The dispatch splits by
interaction type, and the parsers are disjoint so a payload can never match both:

| Type | Parser | Event |
|---|---|---|
| 2 APPLICATION_COMMAND | `parse_interaction` | `Event::Command` |
| 3 MESSAGE_COMPONENT | `parse_component` | `Event::Interacted` |
| 5 MODAL_SUBMIT | `parse_component` | `Event::Interacted` |

Types 3 and 5 exist to serve the `ask_user` question flow — buttons, select
menus and modals. The same re-authorization rule applies to them and matters
more, because a component sits in a channel where anyone can click it: see
`asking-the-user` for that flow and its invariants.

## Archiving is the cross-surface "start over"

`session_for` maps a channel to a conversation through the KV table, and the
mapped conversation is reused only while it is *live*. Archiving is how someone
says they are finished with a transcript — `archive_session` also shuts the
worker down and releases the worktree — so a chat surface that carried on in it
would contradict a decision made deliberately in the web UI and resurrect state
meant to be at rest. An archived mapping therefore yields a fresh conversation,
which makes archiving equivalent to `/new` for every surface at once. Nothing is
destroyed: the old transcript stays readable, and unarchiving before the channel
next speaks brings it back.

The trap is that **archiving is a flag, not a delete**. `get_session` happily
returns an archived session, so testing that the mapped id merely *exists* is not
enough — `store::archiving_only_sets_a_flag_and_get_session_still_returns_it`
pins that property down. The decision lives in `policy::may_reuse_session`, pure
so the archived case does not need a live database, and
`tests/discord_archived.rs` drives a real store through the same steps to check
the end-to-end behaviour. Reverting `may_reuse_session` to `existing.is_some()`
makes those integration tests fail with the same id on both sides of the
assertion, which is what the bug looked like.

## Streaming a reply: two pieces, not one

`stream_reply` in `mod.rs` keeps **`settled`** (the text of assistant steps that
have finished) apart from **`buffer`** (the current step's deltas), and `visible()`
joins them for every write. That split is forced by the events disagreeing about
what they carry:

| Event | Means |
|---|---|
| `StreamDelta` | *append* this fragment to the step in flight |
| `AssistantMessage` | the step's **whole** final text |

Held in one string, `AssistantMessage` overwrites everything the deltas built, so
a turn that narrates, calls a tool, then narrates again loses its opening
paragraph from the channel. Only the *last* step would survive.

Two rules keep the message honest on a long turn:

- **Flush when a step ends,** in the `AssistantMessage` arm, not only on the
  interval and at `TurnFinished`. Deltas flush on a timer, so the text arriving
  between the last tick and the model turning to a tool call is still unsent —
  and a turn that then spends ten minutes in tools leaves a **half-finished
  sentence** on screen for all of it. That is what "streaming broke" looks like
  from the outside, and it needs no error to happen. The step's text is final, so
  there is nothing to wait for; the interval still bounds the streaming case, so
  it costs at most one extra edit per step.
- **Text already said is never replaced by a progress note** — but it must still
  be *joined* by one. The message has three parts, not two, and the third is the
  only retractable one:

  | Part | Meaning | Retracted? |
  |---|---|---|
  | `settled` | steps that have finished | never |
  | `buffer` | the step in flight, appended by deltas | folded into `settled` |
  | `activity` | the tool now running, as a trailing `_… name_` | yes, always |

  `compose(settled, buffer, activity)` renders the three; `visible()` renders the
  first two and is what `TurnFinished` and `Incident` use, which is how a
  finished reply never ends on a stale `_… web_search_`.

  Showing the note **only when nothing had been said yet** was a real bug, not a
  safe conservatism. After the first sentence every tool call became invisible,
  so a turn that spoke one line and then worked for eighteen minutes left that
  line frozen with no edits, no typing indicator and no error. Measured live: one
  message, 146 characters, three sentences, a single edit in eighteen minutes.
  Clear `activity` on `StreamDelta`, `AssistantMessage`, `Incident` and the
  `ask_user` arm — anywhere real text supersedes it.

- **Typing must be driven by an interval owned outside the `select!`,** never a
  `sleep` arm inside it. A future built in a `select!` arm is recreated on every
  iteration, so each event resets it; during a busy turn events arrive every two
  or three seconds and an 8s `sleep` therefore *never* elapses — the indicator
  starves exactly when it is wanted and only appears when idle. A standalone
  probe scores this 0 firings against 2 for `tokio::time::interval`. Call
  `tick()` once before the loop, since the first tick resolves immediately.

`Incident` appends to `settled` and clears `buffer` for the same reason: it
belongs to no step, so the next `AssistantMessage` must not overwrite it.

`visible()` is pure and unit-tested (`discord::tests`), which is the only part of
the streaming path testable without a token — the edit cadence is not.

## Things learned the hard way

- **A stalled-looking reply is usually not a transport fault.** A truncated
  message that never updates means unflushed buffer plus a turn still working,
  not a broken socket. Check whether the worker is still burning tokens
  (`journalctl -u thetis | grep "token usage"`) before touching the stream code:
  a live worker and no `could not edit a Discord reply` warning rules out the
  edit path entirely.
- **Fatal close codes must not be retried.** 4004 (bad token) and 4013/4014
  (missing privileged intents) never succeed, and reconnecting in a loop is how
  an address gets rate-limited. `api::is_fatal_close` stops the connector and
  `api::fatal_advice` says what to change. Everything else backs off
  exponentially to 60s.
- **`MESSAGE_CONTENT` is privileged.** Without it enabled in the Developer
  Portal every message arrives with empty text and the bot looks connected but
  mute. It is the single most common setup failure.
- **Deny `@everyone` and role pings.** Discord parses mentions in whatever the
  bot sends, so any model output containing `@everyone` would ping the server.
  `Rest::allowed_mentions` sends `parse: ["users"]` only.
- **Authorization is checked before the mention rules,** so an outsider cannot
  map the configuration by watching which channels produce a refusal.
- **Attribution is user-controlled input.** `user-msg` in the WIT contract has no
  author field, so identity travels in the text as `[Name] message`. Display
  names are stripped of control characters and brackets, or a name containing a
  newline could forge a second speaker.
- **Heartbeat shares the read task** via `select!`, because the socket is not
  `Sync` and a mutex between two tasks would be worse. A message is handled on
  its own spawned task, since a turn far outlasts the heartbeat interval.

## A reply is several messages, and the split is stateful

Discord rejects a body over 2000 characters. Meeting that by truncating throws
away the **start** of every long answer, which is where the reasoning is — the
symptom is a reply beginning `… (truncated)`. `split::paginate` lays a long reply
out across as many messages as it needs; `api::truncate` survives only as a
backstop for callers that do not paginate, such as ephemeral command replies.

Three properties make it work, and each is load-bearing:

- **Markup state crosses the break.** A page that ends inside a fenced block
  must close the fence and the next must reopen it *with the same info string*,
  or Discord renders the continuation as prose — and an unclosed fence swallows
  the rest of its own message. Same for an odd inline backtick. `Pager` carries
  `open: Option<String>` and `inline: bool` for exactly this.
- **Pagination is prefix-stable.** Pages are packed greedily from the start, so
  appending text can only change the *last* page. This is what lets a streaming
  reply repaginate on every tick without rewriting messages the reader has
  already scrolled past. Break it and every tick edits every message.
- **`split::plan` diffs pages against what was sent** and emits only
  `Edit`/`Send`/`Delete` for what actually differs, deletes descending so
  earlier indices stay valid. A `Send` failure must stop the loop — posting page
  N+1 after N failed shows the answer out of order with an invisible hole.

Do **not** "repair" unbalanced markup in a reply that fits in one message. Mid-
stream text is legitimately unbalanced and resolves itself as more arrives;
normalising it makes fences flicker on every tick. Balance is a property of pages
*this splitter created*, not of the model's text.

## Reading the channel is the fastest diagnosis

A stalled reply is diagnosed in one call, not by reasoning about the code. Fetch
the channel with the bot's own token (see `discord-connector/live-probing`) and
look at `edited_timestamp` and `len` on the bot's message:

- **Sent, one edit, hours old, short** → the loop is alive but suppressing
  updates. A content bug, as above.
- **Sent, never edited** → `flush` is failing; check for `could not edit a
  Discord reply` warnings.
- **Never sent** → the events are not reaching `stream_reply`; check the
  `session_id` filter and the worker's `event` forwarding.

The message body itself tells you which arm ran: three short sentences and no
progress notes means `AssistantMessage` fired three times and every
`ToolInvocation` was dropped.

Verify the pagination against the real API, not just as strings — a 2000-char
body is refused with a 400 and the reply disappears with only a log line. The
`#[ignore]`d `dump_pages_for_a_live_probe` in `split.rs` writes the real
paginator's output to `/tmp/pages.json`, and `/workspace/zero-discord-probe`
posts it, edits the tail as a delta would, reads it back and deletes it. Six
pages of 1951–1991 chars: every `POST 200`, the tail `PATCH 200`, every page read
back balanced. That is the evidence to cite, not the unit tests alone.

## Testing without a token

`policy.rs` is pure, so the routing and authorization rules are unit-tested
directly — that is where the security decisions live. The mode guard is
verifiable against the real binary:

```
DISCORD_BOT_TOKEN=fake THETIS_ROOT=/tmp/dtest DISCORD_MODE=agent ./target/debug/thetis
```

A fake token reaches Discord and returns `4004`, which proves the socket, TLS and
handshake work. The command endpoints can be checked without a token too, and the
status codes are the evidence:

```
curl -o /dev/null -w '%{http_code}\n' -X PUT -H 'Authorization: Bot invalid' \
  -H 'Content-Type: application/json' -d '[]' \
  https://discord.com/api/v10/applications/000000000000000000/commands   # 401
curl -X POST -H 'Content-Type: application/json' -d '{"type":4,"data":{}}' \
  https://discord.com/api/v10/interactions/0/bad/callback                # 404
```

`401` on the first proves the path is right and only the credential is wrong.
`404 Unknown interaction` rather than `401` on the second is the evidence that
the callback wants no Authorization header. `cargo test -p thetis --test
discord_schema -- --nocapture` prints the exact registration payload.

But do not stop there and call the rest unverifiable: **the token is on this
box**, so what the command set actually looks like to Discord is one call away.
See `discord-connector/live-probing`. The check that matters after touching
`schema()` is `GET /applications/{app}/commands` with the real token, reading
`contexts` and `integration_types` off every entry — that is the only thing that
distinguishes a registration Discord accepted from one a user can invoke, and it
is what the 200 hides. What genuinely needs a human at a client is the last hop:
that typing `/help` in a guild produces an `INTERACTION_CREATE`. Its absence in
the journal (`grep -iE "refused a slash command|failed to handle a Discord slash
command"` finding nothing over days, while ordinary chat worked) is what proved
the commands were never reaching the handler in the first place.

## Kernel builds

A long `cargo build -p thetis` in the foreground gets killed when Thetis
restarts. Run it detached and poll:

```
(setsid nohup cargo build -p thetis > /tmp/b.log 2>&1 &)
```

Terminal sessions do not survive a restart either; reopen with `terminal_open`.
