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
- **Do not name `integration_types` or `contexts`.** Omitted, each command
  inherits the application's configured installation contexts. Asking for the
  user-install context on an app that only has guild install makes Discord
  answer "Unknown integration" and fails the whole registration.
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

## Things learned the hard way

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
discord_schema -- --nocapture` prints the exact registration payload. Everything past authentication — streaming edit cadence, `/pair`
round trip, threads — is unproven until a real token exists. Say so rather than
implying it works.

## Kernel builds

A long `cargo build -p thetis` in the foreground gets killed when Thetis
restarts. Run it detached and poll:

```
(setsid nohup cargo build -p thetis > /tmp/b.log 2>&1 &)
```

Terminal sessions do not survive a restart either; reopen with `terminal_open`.
