# Thetis

A self-modifying agentic system. The agent's loop, its tools, and the chat
interface you talk to it through are all WebAssembly components that the agent
can rewrite while it is running. Every change is compiled, validated, versioned,
and reversible.

> Thetis was a shapeshifter. Peleus could only win her by holding on through
> every form she took — fire, water, lion, serpent — until she was herself
> again. The names follow from that: the guests are **aspects**, the forms the
> system takes; the contract that holds them while they change is the
> **[grip](wit/thetis.wit)**.

## How it fits together

One native binary — the **orchestrator** — is the trusted kernel. It owns the
network, the filesystem, the database, and the build toolchain, and hands guests
narrow, mediated slices of them through the contract in [`wit/thetis.wit`](wit/thetis.wit).
Guests hold no ambient authority: everything they can observe or affect is an
import declared there.

Three kinds of guest plug into it:

| Aspect | Source | What it does |
|---|---|---|
| `agent` | [`agents/agent-core`](agents/agent-core) | Runs the agentic loop: rehydrate, prompt, stream, dispatch tools, check for nudges |
| `gateway/web` | [`gateways/gateway-web`](gateways/gateway-web) | Serves the chat UI and owns the wire protocol |
| `tool/<name>` | `tools/<name>` | One tool each, scaffolded and edited by the agent |

Guests are instantiated **per call** in a fresh store. That is what makes hot
swapping safe: a turn already in flight finishes on the component it started
with, and the next call picks up the new one.

## Running it

Requires the Rust toolchain with the `wasm32-wasip2` target:

```bash
rustup target add wasm32-wasip2
```

Set an OpenRouter key and start it. Guests are built automatically on boot.

```bash
OPENROUTER_API_KEY=sk-... cargo run --release -p thetis
```

Then open <http://127.0.0.1:7777>.

### Without an API key

A scriptable stand-in speaks enough of the streaming protocol to exercise the
whole system — token deltas, tool calls, usage accounting — at no cost:

```bash
cargo run --release --bin mock-llm
```

```bash
OPENROUTER_API_KEY=test OPENROUTER_BASE_URL=http://127.0.0.1:7788 THETIS_MODEL=mock/echo cargo run --release -p thetis
```

## The chat surface

The web UI is an ES-module app with no build step, embedded in the gateway
component. Its pieces are small and separable so extending it stays cheap:

```
ui/index.html      the shell
ui/theme.css       design tokens — the only file with colours in it
ui/app.css         layout and components
ui/app.js          entry point: wires the socket, store and views together
ui/lib/socket.js   connection, reconnect, frame routing by type
ui/lib/store.js    client state with per-key subscriptions
ui/lib/dom.js      element helpers
ui/views/*.js      sidebar, transcript, composer, a generic picker and panel
```

Adding a file means one line in `assets.rs`; adding a client action means one
function in `handlers.rs` and one entry in its dispatch table. The mode and
model selectors are both instances of the same `Picker`, so a third control is
a few lines rather than a new widget.

**Attachments.** Images can be pasted, dropped, or picked from the file
browser. They travel base64-encoded from the browser through the event log to
the model's `image_url` content parts, so nothing re-encodes on the way and a
reopened conversation still shows its pictures.

**Modes.** Each conversation has a mode, `Agent` by default. `Plan` withholds
every tool that would change something — and refuses them at dispatch too, so a
model that remembers a tool from earlier in the conversation still cannot call
it. New modes are two entries in `Config::default_modes` plus whatever the agent
makes of the id.

**Models.** A per-conversation override, chosen from `THETIS_MODELS`. Empty
means the grip default.

**Skills.** Named instruction sets, one markdown file each in `skills/`, with a
short frontmatter block for the title and description and the body as the
instructions. Attach them per conversation from the **Skills** panel; attached
skills are appended to the system prompt. Editing a file takes effect on the
next turn — nothing to restart, nothing to register.

```markdown
---
name: Concise replies
description: Answer in as few words as the question needs.
---

Lead with the answer. Do not restate the question.
```

**Tools panel.** Shows exactly what the model is offered for this conversation,
asked of the agent itself rather than reconstructed, so it cannot drift from
reality. Each tool lists its arguments, whether it is built in or a hot-loaded
component, and whether it changes anything — which is also why the list shrinks
in Plan mode.

**Titles.** A conversation is named from its opening message — trimmed to one
line and cut on a word boundary, or named after the file when the message is
only an attachment. This happens in the store, so it applies however the message
arrived, and it only ever replaces the untouched default: once a title exists,
nothing renames it but you.

## What the agent can do to itself

These tools appear in the model's tool list whenever the dev kit is available.
Every mutating one rebuilds the target immediately and returns the compiler's
verdict **in the tool result**, so the model fixes its own build errors inside a
single turn instead of waiting for a human to relay them.

| Tool | Effect |
|---|---|
| `new_tool(name, description)` | Scaffolds a tool crate, builds it, loads it |
| `write_code(target, path, contents)` | Replaces a file, rebuilds, hot-swaps |
| `patch_code(target, path, old_text, new_text)` | Exact-match patch, rebuilds, hot-swaps |
| `read_code` / `list_code` | Inspection |
| `rollback(target, revision?)` | Restores an earlier revision, or the whole system |
| `history(target)` | Revision history |

## What the agent can do to this machine

Off by default in nothing — these are on, but confined. Turn any of them off in
`thetis.toml`.

| Tool | Effect |
|---|---|
| `read_path` / `write_path` / `list_path` / `delete_path` | Files on the host, confined to `filesystem.roots` |
| `terminal_open` / `terminal_run` / `terminal_read` / `terminal_close` / `terminal_list` | Shell sessions that keep their working directory and state between commands |
| `restart_orchestrator` | Replaces the Thetis process, for changes to the native binary or to startup-only settings |

**The roots are the boundary.** Every path is resolved — symlinks followed —
and must land inside `filesystem.roots`, which defaults to the project root
alone. A terminal session starts inside a root for the same reason. The
`protected` list (`data`, `artifacts`, `.git` by default) is a smaller thing: it
stops the system deleting its own state by accident, and is not a security
control, because a shell can reach those paths regardless.

Be clear-eyed about what enabling terminal access means: the agent can run
anything you can. `filesystem.enabled`, `filesystem.allow_delete`,
`terminal.enabled` and `control.allow_restart` each turn a slice of that off.

**Restarting** is scheduled rather than immediate — the call returns, the turn
finishes, and the user reads why before the process goes away. The replacement
is spawned before the old process exits, so a failure to start leaves the
current one running; the new process retries binding while the old releases the
port. Restarts are refused before `control.min_uptime_secs`, so a restart that
fixes nothing cannot become a loop.

**Turns survive a restart.** A turn cut short — by `restart_orchestrator`, a
crash, or anything else — is picked up again when Thetis comes back. Resuming
costs nothing structurally, because the agent is stateless between turns: it
rebuilds its context from the session log every time, so carrying on is just
running a turn again against a log that now records the interruption.

Two things happen before that can work. Tool calls whose results never arrived
are answered with a failure, because a model request carrying tool calls with no
matching results is rejected outright by most providers. And a note is appended
explaining the interruption, which the agent reads as context for why its last
step went missing.

`restart_orchestrator` takes `resume`, defaulting to true. Setting it false ends
the turn at the restart instead, closing it in the log so nothing is left
looking like it is still thinking. A turn that keeps dying stops being resumed
after a couple of attempts, and the count resets whenever a turn reaches its
end.

`target` is `self`, `tool:<name>`, `gateway:<name>`, or (for rollback) `system`.

A tool created this way is callable on the very next loop iteration. A change the
agent makes to *itself* takes effect when the current turn ends.

## Staying recoverable

The system is built so that no self-modification can make it unreachable.

- **Validation gate.** A candidate must compile, load as a component for its
  world, and pass a smoke test — the agent answers a health probe, a gateway
  serves its index page, a tool returns a valid manifest — before it goes live.
  A candidate that fails is recorded and set aside; the running revision is
  untouched.
- **Epoch watchdog.** Every guest call runs under a wall-clock budget and a
  limit on how long it may execute without yielding to a host call. An infinite
  loop in the agent traps in seconds and the process is unharmed.
- **Circuit breaker.** Repeated traps from one aspect roll it back to its last
  known-good revision automatically, and the incident is written into the
  conversation so the user sees what happened.
- **Revisions and snapshots.** Every activation freezes an immutable revision —
  the component *plus* its full source — and records a system snapshot of what
  every aspect was running. Rollback restores source and binary together, so the
  code the agent reads is always the code that is running. Revision 1 of each
  aspect is pinned.
- **`/admin`.** A control panel rendered by the orchestrator itself, with no
  WebAssembly in its path. It lists every aspect's history and every system
  snapshot with one-click restore, and keeps working when every guest is broken.

Writes are constrained too: paths cannot escape an aspect's source tree, and
`Cargo.toml`, `build.rs`, and `.cargo/` are off limits, because a host-side build
executes them. Changing dependencies stays a human decision.

## Configuration

Thetis reads `thetis.toml` from the project root. Every setting is optional —
delete the file and it still runs on built-in defaults — and three layers stack:

1. the defaults compiled in, so nothing is required to start
2. `thetis.toml`, or whatever `THETIS_CONFIG` points at
3. environment variables, for per-run overrides and secrets

The file is parsed strictly: a mistyped key fails at startup naming the key,
rather than becoming a setting that silently does nothing. So does a
`default_mode` that is not among the configured modes.

The shipped file documents every section. The ones worth knowing:

| Section | Covers |
|---|---|
| `[server]` | bind address, which gateway serves the UI, whether `/admin` is on |
| `[paths]` | where the agent, gateways, tools, skills, artifacts and data live, and the naming conventions for gateway and tool directories |
| `[llm]` | base URL, default model, timeout, retries |
| `[agent]` | iteration ceiling, default mode, the system prompt (inline or from a file) |
| `[[models]]` | the model picker's contents |
| `[[modes]]` | ways of working, each with `read_only` |
| `[budgets]` | turn ceiling, the no-yield slice that catches infinite loops, tool and probe budgets |
| `[limits]` | memory caps, spend ceiling, output and attachment sizes |
| `[cache]` | prompt caching: TTL, anchor spacing, which vendors need explicit breakpoints |
| `[build]` | build command, target triple, profile, target directory, `--locked`, extra flags |
| `[watchdog]` | breaker window and threshold, probe interval, watch suppression, debounce |
| `[devkit]` | whether self-modification is offered, and which files guests may never edit |
| `[sandbox]` | the Docker exec sandbox |

Adding a mode is configuration alone. `read_only` is carried through to the
agent, which withholds every tool that changes something and refuses those tools
at dispatch too — so a new read-only mode needs no code in the agent, which
never knows any mode by name.

## Prompt caching

On by default, and the single largest cost lever: a repeat turn in a long
conversation costs about a tenth of a fresh one.

Providers differ in kind, not just in syntax, so the strategy is per vendor:

| Vendor | Behaviour | What Thetis does |
|---|---|---|
| Anthropic | Caches **nothing** unless the request marks where | Writes explicit `cache_control` breakpoints |
| OpenAI | Caches long prefixes automatically | Nothing — marking would only bill writes |
| Google | Implicit on recent models; explicit bills a full-price write plus storage, and only the last mark counts | Left implicit |

Anthropic's cache is a prefix cache over `tools → system → messages`, where a
breakpoint writes one entry covering everything up to that block. The subtlety
that decides whether this works at all: a later request hashes its prefix at
each breakpoint and walks back **at most twenty blocks** looking for a match. A
turn that runs a dozen tools can add more than twenty blocks at once, so a lone
breakpoint at the end would sail past the previous entry and re-read the whole
conversation at full price.

Thetis therefore places up to four breakpoints — the limit Anthropic allows —
on the last system message, on two *anchors* that sit at a fixed stride and so
hold still while the conversation grows around them, and on the final message,
which writes the newest prefix for the next turn to read back.

Breakpoints are applied host-side in `cache.rs`, after the model is resolved and
regardless of what the agent sent. Caching therefore cannot be broken by the
agent rewriting its own loop.

Cache hits show in the transcript under each reply, because a saving you cannot
see is one you cannot trust. Measured on a three-turn conversation with the full
tool surface: the opening turn cost $0.0106, and each turn after it reported
99% of its prompt served from cache at $0.0010.

**The API key** can live in `[llm] api_key` or in `OPENROUTER_API_KEY`, with the
environment winning so a key can be overridden for one run without editing
anything. Blank in either place counts as absent, so an empty setting fails at
startup rather than becoming an empty `Authorization` header.

A key in the file is a key on disk. `.gitignore` excludes `thetis.local.toml`
and `*.local.toml`, so if this repo is ever version controlled, keep the real key
in one of those and point `THETIS_CONFIG` at it. Thetis holds the key in a type
whose `Debug` prints `Secret(***)`, so it cannot reach a log through an
incidental `{:?}` on the config.

### Environment overrides

Anything in the file can be overridden per run. The common ones:

| Variable | Overrides |
|---|---|
| `OPENROUTER_API_KEY` | `llm.api_key` |
| `OPENROUTER_BASE_URL` | `llm.base_url` — point it at the mock for offline work |
| `THETIS_CONFIG` | which config file to read |
| `THETIS_ROOT` | the project root |
| `THETIS_BIND` | `server.bind` |
| `THETIS_MODEL` | `llm.model` |
| `THETIS_MODELS` | the model picker, as `id=Label` pairs |
| `THETIS_DEFAULT_MODE` | `agent.default_mode` |
| `THETIS_SYSTEM_PROMPT` | the system prompt |
| `THETIS_SANDBOX` | `sandbox.enabled` |
| `THETIS_DEVKIT` | `devkit.enabled` |
| `THETIS_LOG` | tracing filter |

Budget and limit values have `THETIS_`-prefixed overrides too, named after
their keys — `THETIS_TURN_BUDGET_SECS`, `THETIS_MAX_ITERATIONS`, and so on.

## Layout

```
thetis.toml             configuration; every path below is one of its settings
wit/thetis.wit          the host/guest contract — changing it rebuilds every guest
crates/thetis           the kernel: loader, pipeline, revisions, watchdogs, web
agents/agent-core        the agent's own source, which it can rewrite
gateways/gateway-web     chat UI and wire protocol
skills/<name>.md         instruction sets you can attach to a conversation
tools/<name>             tools the agent scaffolds for itself
templates/tool-template  what new_tool starts from
artifacts/               immutable revisions (component + source snapshot)
data/thetis.redb        sessions, event log, revision registry
```

## Tests

```bash
cargo test -p thetis
```

Covers the event log, the SSE parser's reassembly of split tool-call arguments,
budget enforcement, breaker thresholds, revision and snapshot semantics, path
traversal rejection, session settings, attachment previews, auto-titling, skill
discovery and attachment, configuration layering and validation, and the
watcher's path-to-aspect mapping.

## Not yet built

- **Docker exec sandbox.** The capability is defined and wired, but the
  implementation is a stub; with `THETIS_SANDBOX=false` the agent is simply not
  offered code-execution tools rather than being handed tools that fail.
- **MCP.** The imports exist and return empty; no client is connected yet.
- Additional gateways (REST, chat platforms) and authentication for the web UI.
