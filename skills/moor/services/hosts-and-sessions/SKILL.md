---
name = "mooR hosts and the session abstraction"
brief = "What moor-telnet-host, moor-web-host and the single-process moor binary each own, and how a Session ties a connection object to a player and keeps output ordered."
when_to_use = "Use when working on a connection endpoint or on player output: telnet listeners and TLS, the web host's HTTP routes, WebSocket or WebRTC, OAuth2, login, attach, reattach and detach, connection objects versus player objects, switch_player, connection attributes, content types, or output that arrives out of order, twice, or not at all. Use it also to decide what the single-process moor binary changes. Not for the RPC transport and tokens themselves (read daemon-and-rpc), not for the FlatBuffer definitions (read wire-schema), not for scrollback and encrypted history (read event-log-and-history), and not for MOO verb code inside a running world such as the Torchship database, which the torchship skills own, or for Thetis internals."
universal = false
tags = ["moor", "telnet-host", "web-host", "session", "websocket", "webrtc", "oauth2", "connection object", "player", "attach", "listeners", "output ordering", "moor-server", "single process"]
version = 1
---

# mooR hosts and the session abstraction

A host owns a socket and a protocol. It does not own state. Every host is a thin
translator between one wire convention and the daemon's typed request/reply and
event vocabulary. The `Session` is the daemon-side object that lets a running
task speak back to whoever asked.

## The three hosts

| Host | Crate | Owns |
|---|---|---|
| Telnet | `crates/telnet-host` | TCP and optional TLS listeners, line framing, telnet option negotiation, reverse DNS of the peer, and rendering djot to a terminal |
| Web | `crates/web-host` | An axum HTTP router, WebSocket sessions, optional WebRTC data channels, OAuth2, CORS, rate limiting, and content negotiation |
| Single process | `crates/server` | Runs the daemon runtime plus both hosts plus selected workers in one process, with typed in-process adapters instead of ZeroMQ |

A host's own crate holds no world logic. If you find yourself wanting world state
in a host, the answer is a new RPC, not a cache.

## What a host does at startup

1. If its RPC address is `tcp://`, enrol and load CURVE keys. See
   `daemon-and-rpc`.
2. Create its host services: an RPC client plus subscription factories.
3. Start its listener set and register its initial listeners.
4. Send `RegisterHost` with a fresh host UUID, its host type and its listener
   list. Retry every five seconds until the daemon acknowledges; exit if the
   daemon rejects.
5. Subscribe to the host broadcast topic and run the host event loop: answer
   pings with a pong that re-sends the listener list, and act on listen and
   unlisten broadcasts.

The listener list is re-sent on every pong. That is deliberate: the daemon's
registry of listeners is soft state, rebuilt from whatever the hosts report, so a
daemon restart recovers the listener set without a persisted file.

## Connections, players and clients

Three identities, often confused.

| Identity | Type | Lives in |
|---|---|---|
| Client id | UUID | One per socket. It is the pub/sub topic and the subject of the `ClientToken` |
| Connection object | Negative `Obj`, allocated downwards from the first connection id | The connection registry. It exists before login and is what MOO code sees as an unauthenticated connection |
| Player object | Positive `Obj` | The world database. It is what an `AuthToken` carries |

Login associates a client id with a player. One player may have many client ids
at once, which is how multi-session play works; `notify()` to a player fans out
to every one of them.

The registry also stores, per client, a **history owner** separate from the
active player. `switch_player()` can move the active player while leaving history
attached to the original, which is how an admin can act as another character
without writing into that character's private scrollback.

Connection records are persisted (Fjall, under the daemon data directory) and
survive a daemon restart, even though the sockets do not.

## The listener handler object

Every listener carries a **handler object** alongside its socket address. That
object, not `#0` by convention, is where the daemon starts every verb that a
connection on that listener triggers.

| Moment | Verb called on the handler object |
|---|---|
| Login attempt | `do_login_command`, with the connection object as the caller |
| Login succeeded | `user_connected`, `user_reconnected` or `user_created` |
| Last connection for a player goes away | `user_disconnected` |
| Every command typed | The command task, which tries `do_command` on the handler before the internal parser |
| Out-of-band input | The out-of-band task |

Where it is set:

- A listener created from the host's own command line uses `SYSTEM_OBJECT` (`#0`).
  Both the telnet host and the web host hard-code that for their startup
  listener. So a default deployment behaves exactly like a classic MOO.
- A listener created from MOO with `listen(handler_object, port, ...)` uses
  whatever object the MOO code passed. The daemon broadcasts that object with the
  listen event; each host stores it per listener and stamps it on every
  connection accepted there.
- The web host's webhook endpoints always use `#0`, whatever the listener said.

The handler object travels with the connection: it is in `ConnectionEstablish`,
`LoginCommand`, `Attach` and every command, and the host never substitutes its
own. This is how one world can serve two front doors with different login
policies on different ports.

**The failure a new world actually meets.** The daemon does not check that the
handler object exists or that it has the verbs, either when the listener is
created or when a connection arrives. The listener opens, the port accepts, the
socket connects — and then the first login attempt submits a task that fails at
verb lookup. The client sees a login failure and the daemon logs a login-task
error. If `do_login_command` exists but returns something that is not an object,
the login is reported as failed rather than as an error. Both look identical from
a telnet prompt: the world is up, the port answers, and nobody can get in.

When you meet that, check three things in order: that the handler object number
in `listeners()` is the object you meant; that it actually exists in the
database; and that it defines `do_login_command`. A core that was imported
incompletely, or a `listen()` call passing an object that was later recycled,
produces exactly this symptom. See `moor/content-pipeline/cores-and-bootstrap`
for what a core is expected to provide.

## What a Session is

`Session`, defined in `moor_common::tasks`, is the daemon-side handle a running
task uses to talk to whoever asked for the work. There are three implementations:

| Implementation | Used for |
|---|---|
| `RpcSession` | A real client connection. Publishes to the client and writes to the event log |
| `OutputCaptureSession` | Unauthenticated system verb calls with no connection; output is collected in memory and returned with the result |
| A background session | Server-side tasks such as the `server_started` hook, made by `RpcServer` as a `SessionFactory` |

A session is *not* a connection. It is bound to a client id and a connection
object, but a fork of a task gets a fresh session sharing the same identity, and
a background session has a client id nothing is subscribed to.

## Buffering, and why it exists

`RpcSession` holds two transaction-local buffers:

- `send_event` appends to the **transaction buffer**: events to be both logged
  and published.
- `log_event` appends to the **log-only buffer**: events written to history but
  never shown live.

Neither is sent when it is produced. On `commit()`, both buffers drain: every
event is written to the event log under the session's history owner, and the
publish buffer is handed to the daemon as one batch. On `rollback()`, both are
cleared.

This is the whole reason output is not duplicated when a transaction retries.
Read `moor/storage-and-state/transactions` for when a retry happens. If you add a
new kind of output, put it in a buffer unless you can prove it must escape a
rollback.

Four things deliberately bypass the buffer and go out immediately:
`send_system_msg`, `request_input`, `set_connection_attribute` and `disconnect`.
They are not narrative, and a rollback should not swallow them.

## How ordering is preserved

1. Within a transaction, the buffer preserves the order the events were produced.
2. `commit()` sends the whole batch as one message to the daemon's session
   mailbox, an unbounded channel drained by a single thread, so batches are
   processed in the order the commits happened.
3. That thread resolves the player to client ids and pushes each event into the
   per-client retention buffer, which assigns a contiguous sequence number under
   one lock.
4. The host refuses any event whose sequence is not exactly one more than the
   last delivered, and asks for a replay instead.

So ordering is enforced twice: once by the batch and once by the sequence number.
Task completion events go through the same mailbox, so a task's result cannot
overtake the output it produced.

## The telnet host

Line-oriented and stateful. A connection is a socket, and losing the socket loses
the session. It advertises two acceptable content types, `text_djot` and
`text_plain`, and renders djot to the terminal itself, with width, UTF-8 and
screen-reader options carried as connection attributes.

The telnet host does not implement event log history. It never sends a history
request and cannot supply an encryption key, so a telnet user sees live output
only. Events from a telnet session are still logged if the player has a public
key configured; the client simply cannot read them back.

## The web host

Two very different shapes share one router.

**Persistent sessions.** `/ws/attach/connect` and `/ws/attach/create` upgrade to
a WebSocket, log in or attach, and then run a bidirectional loop. Optionally the
client offers a WebRTC data channel over the same WebSocket as signalling; events
whose domain is in the configured realtime set then go over the data channel,
which can be unordered and unreliable for latency. Everything else stays on the
WebSocket.

**Ephemeral requests.** Every `/v1/...` REST call is a whole connection: an
extractor attaches to the daemon with the caller's `AuthToken`, and a guard sends
a detach when the handler returns, on the success path and the error path alike.
This is why a REST call has real session semantics rather than a side channel
into the scheduler.

Other web-host concerns: content negotiation between `application/x-flatbuffers`
and `application/json` on the same endpoints; OAuth2 providers as an optional
router; CORS, request body limits and optional per-IP rate limiting on the auth
endpoints, with a trusted-proxy CIDR list deciding which address counts.

The browser client itself (Meadow, `clients/meadow`) and the shared TypeScript
protocol layer (`clients/web-sdk`) are separate npm workspaces. They are not
hosts; they only talk to the web host.

## The single-process binary

`crates/server` builds the `moor` binary. It constructs the same daemon runtime,
the same telnet host and web host and optionally embedded workers, then replaces
the transport:

- `LocalEventBus` implements the daemon's `Transport` trait with in-process
  broadcast channels. Its request loop does nothing, because there is no socket.
- `LocalRuntimeClient` implements the host-side `RuntimeClient` by calling the
  daemon's `RuntimeApi` directly.
- `LocalRuntimeServices` and `LocalWorkerServices` supply the subscriptions.

No ZeroMQ, no FlatBuffers and no enrollment are involved. The endpoint strings in
the code still look like `inproc://` addresses; they are vestigial labels, not
sockets. Session semantics, tokens, connection objects and buffering are
unchanged, which is the point: single-process mode is a different adapter, not a
different server.

Two documents describe this differently and are behind the code.
`book/src/the-system/server-architecture.md` says single-process components
"communicate through in-process ZeroMQ endpoints"; they no longer do.
`single-process-deployment.md` says the binary lives at
`crates/daemon/src/bin/moor.rs`; it lives at `crates/server/src/main.rs`.

## Invariants

1. **A host holds no authoritative state.** Everything it knows it was told, and
   it must be able to lose it.
2. **A host never calls the scheduler.** It calls the runtime boundary. This is
   what keeps single-process and split-process behaviour identical.
3. **Narrative output leaves the session only at commit.** Anything published
   earlier duplicates on retry.
4. **Per-client event sequence numbers are contiguous.** A host that cannot
   satisfy this must request a replay, not skip.
5. **The daemon's listener registry is soft state.** Hosts re-assert it on every
   pong; never persist it.
6. **A connection object is negative and a player object is positive.** Code that
   assumes a positive `Obj` for a connected user is wrong before login.
7. **One player may have many clients.** Never assume a single client id for a
   player.
8. **The handler object travels with the connection.** A host stamps the
   listener's handler object on every request and never substitutes `#0` for it.
   Nothing validates that object, so an invalid one fails at the first login and
   not before.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/daemon/src/rpc/session.rs` | `RpcSession`: buffers, commit, rollback, fork, identity |
| `crates/daemon/src/rpc/output_capture_session.rs` | In-memory session for connection-less calls |
| `crates/daemon/src/connections/` | Client to connection to player mapping, history owner, persistence |
| `crates/telnet-host/src/lib.rs` | Host startup, registration, host event loop |
| `crates/telnet-host/src/listeners.rs` | Listener set, accept loop, TLS, connection establish |
| `crates/telnet-host/src/session/` | Codec, telnet negotiation, djot rendering, MOO highlighting |
| `crates/web-host/src/routes.rs` | The router, CORS, rate limiting, body limits |
| `crates/web-host/src/host/web_host.rs` | Attach, reattach and the connection decision matrix |
| `crates/web-host/src/host/session/` | The WebSocket loop and the WebRTC peer |
| `crates/web-host/src/host/auth/` | Login, the ephemeral attach/detach extractor, OAuth2 |
| `crates/web-host/src/host/negotiate.rs` | FlatBuffers versus JSON response format |
| `crates/web-host/src/host/handlers/` | The REST endpoints |
| `crates/zmq-client/src/host.rs`, `host_services.rs` | Split-process host session and subscriptions |
| `crates/server/src/local_*.rs` | The in-process adapters |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Host exits at startup: "Daemon has rejected this host" | The daemon refused registration | Read the reason in the log. This is fatal by design; the host does not retry a rejection |
| Host retries registration forever | The daemon is not reachable | Check the RPC address, and CURVE enrollment for `tcp://` |
| Output appears twice after a busy moment | Something published before the world-state commit | Move it into the session's transaction buffer |
| Output vanishes on a conflicting transaction | Correct behaviour for narrative; wrong if the message was a system notice | Use `send_system_msg`, which bypasses the buffer |
| A user sees another user's scrollback after `switch_player` | History owner was moved with the active player | Pass the preserve-history option; see the history owner rules in `event-log-and-history` |
| WebSocket connects, then no events | The subscription was never established, or the replay layer is missing | The host must wrap its live subscription in the recovering subscription; a raw subscription silently loses events |
| REST call returns 503 | The daemon could not be reached during ephemeral attach | The daemon is down or the RPC socket is wrong |
| REST call returns 401 on a token that just worked | The connection went stale, or the token's player no longer matches | Re-authenticate; do not extend token lifetime as a workaround |
| `listeners()` in MOO lists a dead port | A host died and the host timeout has not elapsed | Wait, or restart the host |
| Telnet user asks for scrollback and gets nothing | The telnet host does not implement history | Expected. Use the web client |
| The port answers but no login ever succeeds | The listener's handler object is missing, or has no `do_login_command` | Check the handler object in `listeners()`, that it exists, and that it defines the verb. Nothing checks this at listen time |

## Read first / read next

- Read `moor/storage-and-state/transactions` before changing anything about when
  a session flushes.
- Read `daemon-and-rpc` for tokens, enrollment and the sequence protocol.
- Read `event-log-and-history` for what happens to the buffered events after they
  are logged.
- Read `moor/working-in-the-repo/build-and-run` for how to run a daemon plus
  hosts locally; `process-compose.yaml` is the split-process reference layout.
  For a real deployment of the same shapes, read
  `moor/working-in-the-repo/deployment-and-release`.
- Read `clients-and-web-ui` for what a browser or app client may assume of the
  web host described here.
