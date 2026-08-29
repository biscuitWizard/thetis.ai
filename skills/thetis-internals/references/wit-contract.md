# The WIT contract

Written in ASD-STE100 Simplified Technical English.

The file is `wit/thetis.wit`, package `thetis:grip@0.1.0`. It is the
boundary between the trusted orchestrator and every hot-loaded guest.

A guest holds **no** ambient authority. Everything it can observe or affect is an
import that is declared here. This is the one document that tells you what you
can and cannot do.

An edit here forces a rebuild of every guest. The dev kit cannot reach the file,
because path confinement keeps a write inside the source tree of the aspect. Read
`careful-surgery/contract-changes` before you plan such a change.

Remake the interface list with:
`grep '^interface\|^world' wit/thetis.wit`

## The interfaces

### `types`

Every shared record and variant. The parts that matter most:

- `token-usage` — prompt tokens, completion tokens, cost, and also
  `cached-tokens` and `cache-write-tokens`, which is how a cache saving becomes
  visible in the transcript.
- `attachment` — `name`, `mime`, `data-base64`. The data stays base64 from the
  browser to the model API, so nothing re-encodes on the path between.
- `session-event` — the variant that the log stores. See the snapshot reference
  for the list of cases.
- `seq-span` and `compaction` — what a summary stands for. Several spans,
  because preserved user messages break the region into pieces.
- `session-meta` — includes `mode` and `model`, the per-conversation overrides.
- `skill-card`, `skill-body`, `skill-diagnostic`, `skill-write`.
- `compile-report`, `dependency`, `revision-info`, `mod-target`,
  `rollback-target`.
- `tool-manifest`, `agent-manifest`, `gateway-manifest`, `asset`.

### `sys`

`log`, `now-ms`, `kv-get`, `kv-put`, `config-get`, `list-models`, `list-modes`.

The KV scope is either `"global"` or a session id. `config-get` is a small
allow-list, not the whole configuration; the snapshot reference lists the keys.

### `session`

`events`, `append`, `emit-output`, `emit-reasoning`, `poll-inbox`,
`list-sessions`, `get-session`, `create-session`, `rename-session`,
`archive-session`, `submit`, `set-session-mode`, `set-session-model`,
`available-tools`.

`emit-output` streams a fragment to connected clients without persisting it.
`emit-reasoning` does the same for a reasoning fragment, kept separate so
thinking is never spliced into the assistant message.
`available-tools` asks the agent itself, so the Tools panel cannot drift from
what the agent really offers.

### `skills` and `skills-view`

`skills` is the agent interface and can write: `universal`, `retrieve`, `search`,
`pinned`, `pin`, `fetch`, `upsert`, `remove`, `lint`.

`skills-view` is the gateway interface and can only read: `all`, `universal`,
`pinned`, `lint`. The split exists so that a gateway can show what the agent
knows without being able to change it.

### `llm`

`chat` for a single non-streaming call that returns the raw provider JSON.
`stream-open`, `stream-next`, `stream-close` for streaming. The host owns the
socket and the API key, accumulates partial tool-call deltas, and enforces the
spend ceiling.

### `sandbox`

`exec`, `write-file`, `read-file`, `list-files`, `available`. It runs a command
inside the Docker container of the session, never on the host. The
implementation is still a stub.

### `tooling`

`registry`, `invoke`, `mcp-list-tools`, `mcp-call-tool`. The MCP functions return
empty; no client is connected.

### `hostfs`

`available`, `read-file`, `write-file`, `list-dir`, `delete-path`. These touch
the machine the orchestrator runs on, so the whole interface is off unless the
configuration turns it on. The real boundary is the configured roots: each path
is resolved and must land inside one, checked after symlinks are followed.

### `terminal`

`available`, `open`, `run`, `read`, `close`, `sessions`.

### `control`

`available`, `restart`.

### `configuration`

`settings`, `get`, `set`.

## The three worlds

| World | Imports | Exports |
|---|---|---|
| `agent` | `sys`, `session`, `skills`, `llm`, `sandbox`, `tooling`, `devkit`, `hostfs`, `terminal`, `control`, `configuration` | `handle-turn`, `health`, `describe`, `list-tools` |
| `gateway` | `sys`, `session`, `skills-view` | `serve-asset`, `on-client-message`, `render-event`, `describe` |
| `tool` | `sys`, `sandbox` | `describe`, `invoke` |

Note what each world does **not** get. A gateway has no `llm` and no `devkit`. A
tool has only `sys` and `sandbox`; it cannot reach the network, the filesystem or
the session log. A tool gets its own `[tools.<name>]` block as `config-json`, or
`{}` when it has none. It never sees the settings of another tool, or anything
else in the configuration.

`health` is the liveness probe that the watchdog and the validation gate use.

## Why an edit here is the highest risk

A change to a record breaks every guest at **instantiation**, not at compile
time. The build succeeds and the failure appears afterwards. This is the one
class of change where a green build tells you nothing.
