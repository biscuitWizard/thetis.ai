---
name = "The mooR daemon and its RPC layer"
brief = "How moor-daemon serves hosts and workers over ZeroMQ: request/reply versus pub/sub, CURVE enrollment, PASETO tokens, and how an event reaches a player."
when_to_use = "Use when working on the daemon or the RPC between processes: an RPC message, host registration, or enrollment. Use it when a host cannot enroll, an event never arrives, or a client is dropped for a backlog. Not for the FlatBuffer definitions (read wire-schema), and not for what a host does with an event (read hosts-and-sessions) or worker dispatch (read workers)."
universal = false
tags = ["moor", "daemon", "rpc", "zeromq", "curve", "zap authentication", "allowed-hosts", "paseto", "clienttoken", "authtoken", "enrollment", "pub-sub", "client token", "auth token", "listeners", "moor-daemon", "sequence numbers", "ping timeout"]
version = 2
---

# The mooR daemon and its RPC layer

The daemon is the only process that opens the world database, runs the scheduler
and executes MOO code. Everything else asks it for something over ZeroMQ. This
skill covers that boundary: who may speak, what a message is authorised to do,
and how an event travels from a committed transaction to a connected player.

## What the daemon owns, and what it refuses

`crates/daemon/src/lib.rs` builds the whole process in one function. It opens the
database, optionally imports a core, starts the scheduler, starts the RPC
transport, the workers server and (for TCP deployments) the enrollment server,
then runs the session mailbox loop until the RPC thread exits.

The daemon owns: world state, the scheduler and VM, connection records, the task
registry, the event log, the set of registered hosts and their listeners, the set
of attached workers, and the signing keys.

The daemon deliberately does not own: any listening socket a player connects to,
any outbound network call, and any filesystem access on behalf of MOO code. Those
are a host's job and a worker's job. Keep it that way. A blocking call added to
the daemon blocks the scheduler.

## Two transports, two roles

| Pattern | Socket | Direction | Carries |
|---|---|---|---|
| Request/reply | ROUTER/DEALER in front of REP workers | Host or worker calls the daemon | Every command, query, login, attach and worker result |
| Publish/subscribe | PUB on the daemon, SUB on the peer | Daemon pushes to hosts, clients and workers | Narrative events, task results, pings, listen/unlisten, worker dispatch |

There are four endpoint groups, all configurable on `moor-daemon`:
`--rpc-listen`, `--events-listen`, `--workers-request-listen` (the PUB side to
workers), `--workers-response-listen` (the REP side from workers) and
`--enrollment-listen`. Each may be a comma-separated list. The transport splits
the list into IPC endpoints and TCP endpoints and binds a separate socket pair
for each group, because they have different security.

The pub/sub side uses three well-known topic bytes plus one per-client topic: the
client's UUID is the topic for events addressed to that client, and the constants
in `moor_runtime_api` name the broadcast topics for all clients, all hosts and
all workers.

## Transport security follows the endpoint scheme

The rule is mechanical and lives in `crates/daemon/src/lib.rs` and
`crates/zmq-client/src/enrollment_client.rs`:

- An endpoint starting with `tcp://` gets CURVE encryption and ZAP
  authentication. The daemon starts a ZAP handler before it creates any CURVE
  socket, and sets the ZAP domain `moor` on each server socket.
- Anything else (an `ipc://` Unix socket) gets no encryption. Access control is
  filesystem permissions.

Do not mix assumptions. A host configured with an `ipc://` RPC address skips
enrollment entirely; the same host pointed at `tcp://` must enrol first.

## Enrollment: how a host earns the right to connect

Enrollment exists only to bootstrap CURVE. It is a separate REP socket that
accepts unauthenticated connections.

1. On first run the host or worker generates a CURVE keypair in its `--data-dir`.
2. It sends an enrollment request carrying the shared enrollment token, its
   Z85 CURVE public key, a service type string and a hostname.
3. The daemon compares the token against the file named by
   `--enrollment-token-file` (default under the XDG config directory). If it
   matches, the daemon mints a service UUID and writes the public key into the
   allowed-hosts directory, one file per service UUID.
4. The daemon replies with the service UUID and its own CURVE public key.
5. The host saves that identity beside its keypair and never enrols again.
6. From then on the ZAP handler answers every CURVE connection by looking the
   presented public key up in the allowed-hosts registry.

The host finds the token, in order: an explicit argument, the file named by
`--enrollment-token-file`, the XDG default token file, then the
`MOOR_ENROLLMENT_TOKEN` environment variable. Enrollment retries with exponential
backoff for roughly thirty seconds, because the daemon may not be up yet.

Rotating the token (`moor-daemon --rotate-enrollment-token`, or the in-world
system-control call) only affects *new* enrollments. Already-enrolled peers keep
working, because their authority is the file in the allowed-hosts directory, not
the token. To revoke a peer, delete its allowed-hosts file.

## Tokens: what they prove, and what they do not

Two PASETO v4 public tokens exist. Both are signed by the daemon's Ed25519 key
and are distinguished by a footer constant, so one cannot be replayed as the
other.

| Token | Minted by | Carries | Proves |
|---|---|---|---|
| `ClientToken` | The daemon, on connection establish | The client UUID | This request comes from that client session |
| `AuthToken` | The daemon, on successful login | The player object id | This request acts as that player |

Almost every client request presents a `ClientToken`; anything acting as a player
also presents an `AuthToken`. Validation is in
`crates/daemon/src/rpc/message_handler_auth.rs`, with a sixty-second positive
cache per token so a busy connection does not re-verify a signature on every
command.

Two limits matter and are easy to get wrong:

- A valid `AuthToken` proves only that the daemon once signed that player id. It
  does not prove the object still exists or is still a player. The daemon carries
  an explicit note about this. Authorisation of the MOO operation itself happens
  in the VM; read `moor/execution/permissions-and-security`.
- There is no host token and no worker token. Older prose (`doc/messaging.md`,
  `doc/RPC_API_SPEC.md`) describes `HostToken` and `WorkerToken` with their own
  footers. No such type exists in the code. A host's and a worker's identity is
  its CURVE key over TCP, and filesystem permission over IPC. If you were
  planning around host tokens, stop.

## How an event reaches a player

This is the path worth memorising.

1. MOO code calls `notify()`. The VM hands a `NarrativeEvent` to the task's
   `Session`.
2. `RpcSession` (in `crates/daemon/src/rpc/session.rs`) puts it in a
   transaction-local buffer. Nothing is sent yet.
3. The task's world-state transaction commits. The scheduler then calls
   `Session::commit()`, which drains the buffer, writes each event to the event
   log, and sends the whole batch to the daemon's session mailbox. A rollback
   clears the buffer instead, so a retried transaction does not duplicate output.
4. The daemon's main loop takes the batch, resolves the player to its live client
   ids through the connection registry, and calls `publish_client_event` for
   each.
5. `ClientEventBuffer` assigns the next sequence number for that client under one
   lock, encodes the event once, and retains it.
6. The transport publishes the encoded bytes on the PUB socket with the client's
   UUID as the topic.
7. The host's subscription delivers it to the user.

Not everything is buffered. `send_system_msg`, `request_input`,
`set_connection_attribute` and `disconnect` go to the mailbox immediately rather
than waiting for the commit, because they are not part of the narrative.

## Delivery is not guaranteed, so it is acknowledged

ZeroMQ PUB drops a message when a subscriber's queue is full, and a successful
send confirms nothing. The daemon therefore retains each targeted client event
until the host acknowledges a later sequence number.

- The host detects a gap in sequence numbers, or five seconds of silence, and
  sends a replay request carrying its `ClientToken` and its last delivered
  sequence. The daemon acknowledges everything up to that sequence and returns
  the next retained events.
- Retention is bounded per client and in total. The exact limits are constants at
  the top of `crates/daemon/src/rpc/client_event_buffer.rs`; read them there
  rather than trusting a number written here.
- A client that exceeds a limit is disconnected. This is deliberate: an
  unbounded backlog would be a memory attack.
- The retention buffer is in memory only. A daemon restart loses it.
- Broadcasts to all clients are *not* covered by this protocol. A ping may be
  dropped.

## Liveness

A background thread pings every five seconds. Hosts answer with a pong that
re-sends their listener list; a host that misses the host timeout has all its
listeners removed from the daemon's registry, which is how `listeners()` in MOO
stops advertising a dead host. Clients answer through their host, and a client
that stops answering is removed from the connection registry. Workers ping on the
same cadence; see `workers`.

## Durability inside the daemon's data directory is not uniform

The daemon opens four separate Fjall databases under its data directory, and they
do not have the same durability. An operator must know this before choosing a
backup or a shutdown procedure.

| Database | Written by | Synchronous fsync |
|---|---|---|
| World state (`world.db`) | The transaction engine | **Never.** Nothing in `crates/db` requests a synchronous persist outside its own tests |
| Event log (`--events-db`) | The event log's background persistence thread | **After every narrative event.** The thread writes the record and then persists with a full sync |
| Connections (`--connections-file`) | The connection registry | On open, and on the periodic compaction the daemon runs roughly every five minutes |
| Persistent tasks (`--tasks-db`) | The suspended-task store | On compaction |

The consequence is counter-intuitive. On an unclean kill, the *history* of what
was said is the most durable thing the daemon holds, and the *world* is the least
durable. Durability of the world comes from the checkpoint mechanism and from the
storage engine's own recovery, not from an fsync on commit; that side of the line
belongs to `moor/storage-and-state/storage-engine`.

Two operational points follow:

- Shut the daemon down cleanly. `SIGUSR1` triggers an emergency checkpoint and
  then an orderly shutdown; the ordinary shutdown path drains the scheduler
  before the process exits. A `SIGKILL` is not equivalent.
- The per-event fsync on the event log is a real write cost. It is one reason
  event logging is off by default, and a reason to put the events database on
  storage that can take it.

### Backing up and restoring the four databases

Because the four databases flush independently and nothing coordinates them,
**there is no ordering that makes a copy taken from a running daemon
consistent.** Any hot copy captures four different moments. Do not look for a
safe sequence; there is not one. Stop the daemon, archive the whole data
directory, start it again. `moor/working-in-the-repo/deployment-and-release`
holds that procedure and the export-based alternative.

What matters instead is knowing which of the four you cannot afford to lose,
because that decides what to do when a restore turns out to be mismatched:

| Database | If it is lost or inconsistent |
|---|---|
| World state | Fatal. This is the world. Restore it, or restore from an export snapshot |
| Event log | Irreplaceable. Nothing else holds player history, and it cannot be reconstructed from the world |
| Persistent tasks | A convenience. Losing it loses suspended tasks and nothing else. It only exists when persistent tasks are enabled |
| Connections | Disposable. It is soft state: hosts re-register, users reconnect, and the daemon prunes stale records anyway |

So a restore ranks: world state and event log together, tasks if you have it,
connections last and optional. All four are created on first open, so deleting a
mismatched connections or tasks database and letting the daemon rebuild it is a
legitimate repair. Deleting either of the first two is not.

## Listeners are pushed, not pulled

MOO code calling `listen()` reaches `SystemControlHandle::listen`, which
broadcasts a listen event to every host. Each host decides whether it is the
right host type and opens the socket. `unlisten()` is the mirror. Note that the
in-world `listen()` currently accepts only the `tcp` host type; a websocket
listener cannot be created this way.

## Invariants

1. **The daemon is the only writer of world state.** A host or worker that
   reaches the scheduler directly breaks session semantics, permissions and
   ordering all at once.
2. **Narrative output is released only after the world-state commit.** Anything
   that publishes before commit will duplicate output when a transaction retries.
3. **Every targeted client event has a contiguous per-client sequence number.**
   Skipping, reusing or reordering a sequence number makes the host's recovery
   logic reject the stream.
4. **A token is authentication, never authorisation.** Never grant a MOO
   capability because a token verified.
5. **CURVE is decided by the endpoint scheme, not by configuration.** A `tcp://`
   endpoint without an enrolled key is refused by ZAP, silently, at connect time.
6. **The daemon holds an exclusive lock on its data directory.** Do not point two
   daemons at one directory.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/daemon/src/lib.rs` | Process construction: database, scheduler, transports, servers, shutdown |
| `crates/daemon/src/args.rs`, `feature_args.rs` | Command line and YAML configuration; the source of truth for endpoint defaults |
| `crates/daemon/src/rpc/transport.rs` | ZMQ sockets, the proxy and worker threads, message framing, error encoding |
| `crates/daemon/src/rpc/server.rs` | The coordinator: ping thread, session mailbox loop, background session factory |
| `crates/daemon/src/rpc/message_handler.rs` | The `MessageHandler` trait and shared state; publishing and session events |
| `crates/daemon/src/rpc/daemon_api_impl.rs` | The typed `RuntimeApi` implementation: one arm per request |
| `crates/daemon/src/rpc/message_handler_auth.rs` | PASETO minting and validation |
| `crates/daemon/src/rpc/message_handler_history.rs` | History recall queries |
| `crates/daemon/src/rpc/client_event_buffer.rs` | Sequence numbers, retention, replay, backlog limits |
| `crates/daemon/src/rpc/hosts.rs` | Registered hosts and their listeners, with timeout expiry |
| `crates/daemon/src/connections/` | Connection records, player association, Fjall persistence |
| `crates/daemon/src/enrollment/`, `zap_auth.rs`, `allowed_hosts.rs`, `curve_keys.rs` | The CURVE trust chain |
| `crates/daemon/src/system_control.rs` | What the scheduler may ask of the RPC layer: shutdown, listen, switch player, worker info |
| `crates/runtime-api/src/api.rs` | The typed, transport-neutral request/reply/event vocabulary |
| `crates/runtime-api/src/api_codec.rs` | Typed enum to FlatBuffer and back |
| `crates/zmq-client/` | The peer side: `RpcClient`, subscriptions, enrollment client, worker loop |

`RuntimeApi` is the seam that matters. The daemon's business logic is written
against typed Rust enums; FlatBuffers appear only at the adapter. That is what
lets the single-process binary skip ZeroMQ entirely.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Host logs "Enrollment failed: invalid token" | The token file the host read does not match the daemon's | Compare the daemon's `--enrollment-token-file` with the host's resolution order; the env var is the *last* fallback |
| Host retries enrollment then exits | The enrollment endpoint is unreachable, or the daemon is not up | Check `--enrollment-listen` on the daemon and `--enrollment-address` on the host; retries stop after about thirty seconds |
| Host connects but every call times out, no error anywhere | CURVE handshake refused by ZAP | The public key is not in the daemon's allowed-hosts directory. Delete the host's saved identity and re-enrol, or copy the key file |
| Daemon logs "Invalid request received" | The peer sent a frame the daemon could not decode | A schema mismatch between daemon and peer. Read `wire-schema` |
| Events stop arriving but commands still work | The SUB subscription is gone or the topic is wrong | The recovery layer should replay after five seconds; if it does not, the host is not using `RecoveringClientEventSubscription` |
| "client ... event backlog exceeded its limit" and the client is dropped | The host stopped acknowledging while the daemon kept producing | The host is stuck or has died without closing. Restart it; the user reconnects |
| Replay returns "requested sequence, but replay starts at" | The client asked for events already evicted, or the daemon restarted | The session cannot be recovered. The host must establish a new connection |
| `listeners()` in MOO shows a port nothing answers on | A host died and the timeout has not elapsed | Wait for the host timeout, or restart the host |
| Daemon refuses to start, "Directory lock acquisition failed" | Another daemon has the data directory | Stop it, or use a different `data-dir` |

## Read first / read next

- Read `moor/execution/task-scheduler` before you add a request that runs MOO
  code. Most such requests return a task id, not a result.
- Read `wire-schema` before you touch a message shape.
- Read `hosts-and-sessions` for the other end of every path here.
- `doc/RPC_API_SPEC.md` lists the messages. It is useful, but it still describes
  host and worker PASETO tokens that do not exist, and it points at
  `crates/common/schema/`, which moved to `crates/schema/schema/`.
