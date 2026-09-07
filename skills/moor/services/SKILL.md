---
name = "mooR processes, RPC and hosts"
brief = "The mooR process topology: what the daemon owns, what a telnet or web host owns, and which child skill covers the RPC, the schema, workers, history or MCP."
when_to_use = "Use when the task crosses a process boundary in mooR: deciding whether work belongs in the daemon or a host, or tracing how an event reaches a player. Then read the child skill it points at. Not for MOO verb code inside a game database, not for the object store or the VM (read moor/storage-and-state or moor/execution), and not for Thetis's own internals."
universal = false
tags = ["moor", "daemon", "rpc", "zeromq", "flatbuffers", "telnet-host", "web-host", "workers", "event log", "mcp", "processes", "architecture", "moor-daemon", "moor-telnet-host", "moor-web-host", "curl-worker", "file-worker", "moor-mcp-host", "curve", "enrollment", "paseto"]
children = "auto"
version = 2
---

# mooR processes, RPC and hosts

mooR is one server split into several processes. The daemon owns the world; every
other process is a peripheral that either brings users to the world or does work
the world is not allowed to do itself. This skill holds the topology, which is
common to all its children. Each child holds one layer.

## The children

- [daemon-and-rpc](skill:moor/services/daemon-and-rpc) — process ownership,
  the two ZeroMQ transports, enrollment, CURVE, PASETO tokens, event delivery
  and acknowledgement. Reach for it when a host cannot connect, an event does
  not arrive, or you must add an RPC message.
- [wire-schema](skill:moor/services/wire-schema) — the FlatBuffers layer in
  `crates/schema`. Reach for it when you must change a message, add a field,
  or you have a decode error or version mismatch.
- [hosts-and-sessions](skill:moor/services/hosts-and-sessions) — the telnet
  and web hosts, the single-process binary, and the `Session` abstraction.
  Reach for it when adding a protocol endpoint, debugging lost or misordered
  output, or working on login and attach.
- [workers](skill:moor/services/workers) — the out-of-process capability
  model. Reach for it when MOO code must touch the outside world, or a
  worker never answers.
- [event-log-and-history](skill:moor/services/event-log-and-history) — what
  history is stored, why it is encrypted, and how a reconnecting client
  replays it. Reach for it when working on scrollback or on data retention
  and deletion.
- [mcp-host](skill:moor/services/mcp-host) — what the MCP host exposes to an
  AI assistant, and the safety boundary. Reach for it when wiring an
  assistant into a MOO, or auditing what one can reach.
- [clients-and-web-ui](skill:moor/services/clients-and-web-ui) — what lives
  under `clients/`, and the contract the web host offers a browser or app
  client. Reach for it when building or debugging a client, or deciding what
  a client may assume.

## The topology

| Process | Binary | Owns |
|---|---|---|
| Daemon | `moor-daemon` | The database, the scheduler, the VM, connection records, the task registry, the event log, host and worker registration, and the RPC server |
| Telnet host | `moor-telnet-host` | TCP and TLS listeners, line framing, telnet negotiation, and text rendering |
| Web host | `moor-web-host` | HTTP endpoints, WebSocket and WebRTC sessions, OAuth2, and the browser-facing encoding |
| Worker | `moor-curl-worker`, `moor-file-worker`, any third-party worker | One capability the daemon deliberately does not have |
| MCP host | `moor-mcp-host` | A stdio Model Context Protocol server that logs in as an ordinary player |
| Single process | `moor` | All of the above in one process, with the RPC replaced by typed in-process calls |

The web frontend (Meadow, in `clients/meadow`) is a static browser application. It
is not a mooR process. It talks only to the web host.

## The rule that decides the side

Ask what the work needs.

- **It needs the world state, or it must be atomic with a MOO transaction.** It
  belongs in the daemon. Only the daemon opens the database.
- **It is a network protocol, a terminal convention, a browser concern, or a
  rendering decision.** It belongs in a host. A host never opens the database and
  never calls the scheduler; it calls the runtime boundary and is answered.
- **It blocks, it is slow, or it touches something the operator must be able to
  refuse.** It belongs in a worker. The daemon must never make an outbound
  network call or a filesystem call on behalf of MOO code.

This rule is the reason the split exists. It is also the reason the single-process
binary is not a shortcut past it: `moor` runs the same daemon, the same hosts and
the same session semantics, and only swaps the adapter under them.

## The crates

| Crate | Role |
|---|---|
| `crates/daemon` | The daemon: database, scheduler wiring, RPC server, workers server, connections, event log, enrollment |
| `crates/runtime-api` | The typed, transport-neutral vocabulary every process shares: requests, replies, events, tokens, and the FlatBuffer codec |
| `crates/schema`, `crates/schema-macros` | The `.fbs` sources, the generated bindings, and the conversions to and from MOO domain types |
| `crates/zmq-client` | The peer side of the ZeroMQ protocol: RPC client, subscriptions, enrollment client, worker loop |
| `crates/telnet-host`, `crates/web-host` | The two shipped hosts |
| `crates/server` | The single-process `moor` binary and its in-process adapters |
| `crates/curl-worker`, `crates/file-worker` | The two shipped workers |
| `crates/mcp-host` | The Model Context Protocol server |
| `clients/meadow`, `clients/web-sdk`, `clients/moor-web-mcp` | Browser client, shared TypeScript protocol layer, and a web-backed MCP server. Separate npm workspaces, not server processes |

`runtime-api` is the seam that makes the rest coherent. Daemon logic is written
against its typed enums; FlatBuffers and ZeroMQ appear only in adapters at the
edges. That is what lets the same code run split across machines or inside one
process.

## Shared configuration

Every peer — both hosts, every worker, and the MCP host — takes the same
connection arguments, defined once as `RpcClientArgs` in `moor-runtime-api`: the
RPC address, the events address, the two worker addresses, the enrollment
address, a data directory for its identity and CURVE keys, and an enrollment
token file. All of them also accept a YAML config file that fills the same
fields. When you add a peer, reuse that struct rather than inventing arguments.

The daemon's own defaults are in `crates/daemon/src/args.rs`. Read them there;
do not trust a copy.

## What survives a restart

| Restart | Survives | Is lost |
|---|---|---|
| A host | The world, the player's object, the persisted connection record, the event log | The socket to the user, and the live subscription. The user must reconnect. The daemon drops the host's listeners after the host timeout |
| A worker | The world | Every request that worker had in flight; the daemon fails those tasks with a worker-detached error |
| The daemon | The world state, the connections database, the persistent task database (if enabled), and the event log, all on disk | Every host and worker registration, every live session, and the whole in-memory retention buffer of unacknowledged client events |

The daemon holds an exclusive lock on its data directory. A second daemon on the
same directory refuses to start.

## Knowledge barriers

Before you change anything under `crates/daemon`, `crates/runtime-api`,
`crates/zmq-client` or `crates/schema`, you must already understand:

1. **The MOO value model.** Every request and event carries `Var`, `Obj` and
   `Symbol`. Read `moor/language-and-compiler/value-model`.
2. **Task submission and suspension.** An RPC that runs MOO code returns a task
   id immediately, not a result. Read `moor/execution/task-scheduler`.
3. **Transactions.** Output is buffered until the world-state transaction
   commits. Read `moor/storage-and-state/transactions`.
4. **Who is allowed to do what.** The RPC layer authenticates a player; it does
   not authorise a MOO operation. Read `moor/execution/permissions-and-security`.

Without 2 and 3 you will write an RPC handler that appears to work and loses
output under retry.

## Read first / read next

- Read `moor/working-in-the-repo/build-and-run` to start a daemon and a host
  locally, and `moor/working-in-the-repo/deployment-and-release` to run one for
  real: compose and process-compose layouts, keys and enrollment tokens,
  packaging and backups.
- Read `moor/working-in-the-repo/testing` before you change a message type; the
  RPC integration tests in `crates/daemon/src/testing` are the fast check.
- `doc/messaging.md` and `doc/RPC_API_SPEC.md` are the prose overview. Both are
  behind the code in places; the children say where.
