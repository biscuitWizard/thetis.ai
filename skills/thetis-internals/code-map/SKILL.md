---
name = "Where to find things in the source"
brief = "A map of the Thetis repository: which orchestrator module owns which behaviour, the guest source trees, and what the dev kit refuses to let you write."
when_to_use = "Use when you must find the file that owns a behaviour, before you read or patch anything: the host imports, the build pipeline, the branch machinery, the prompt cache, the store, the web layer, the terminal, or a guest source tree. Use it also to check whether a path is writable by the dev kit. Not a substitute for list_code and read_code, which give the current truth."
universal = false
tags = ["source map", "repository layout", "files", "modules", "orchestrator", "crates", "where is", "host_api", "pipeline", "tool-group:selfmod"]
related = ["careful-surgery"]
version = 3
---

# Where to find things in the source

The project root holds `thetis.toml`. Each path below is one of its settings, so
confirm a path with `list_config` if it is not where you expect it.

## Top level

| Path | Content |
|---|---|
| `wit/thetis.wit` | The host and guest contract. An edit here rebuilds every guest. |
| `crates/thetis` | The native kernel. |
| `agents/agent-core` | Your own source. `target: self` in the dev kit. |
| `gateways/gateway-web` | The chat UI and the wire protocol. |
| `tools/<name>` | One tool each. |
| `templates/tool-template` | What `new_tool` starts from. |
| `skills/<id>/SKILL.md` | The skill corpus. |
| `artifacts/cache/` | Built components and kernels, keyed by the source tree that produced them. Shared across every branch. |
| `data/thetis.redb` | Sessions, the event log, the branch registry, the KV store. Opened only by the gateway. |
| `workspace/` | The only directory the WASI guests get, from `wasi.dirs`. Shared across every branch. |
| `worktrees/` | One checkout per conversation. **You are inside one of these**: your `cargo`, your edits, and your git history are this conversation's alone. |

## Your own source: `agents/agent-core/src`

| File | Content |
|---|---|
| `lib.rs` | The `Turn` struct, `handle_turn`, rehydration, the system prompt, streaming, dispatch, the inbox. |
| `compaction.rs` | The `Policy`, round and span selection, the summary call. |
| `tools.rs` | Every built-in tool: the definition, the JSON schema, the mutating flag, and dispatch. |

To add a built-in tool you make two edits in `tools.rs`: a `ToolDef` in the
correct group function, and an arm in `invoke`. Set `mutating` correctly, because
that is what a read-only mode filters on.

## The orchestrator: `crates/thetis/src`

Grouped by concern.

**The contract and the runtime**

| File | Owns |
|---|---|
| `bindings.rs` | The generated bindings for the three worlds. The `agent` world is canonical; the others reuse its types. |
| `runtime.rs` | The engine, the per-call store, the capability-scoped linker, and the epoch budget. |
| `loader.rs` | The registry of the component that is active in each aspect. A swap is a pointer replacement. |
| `aspect.rs` | Aspect identity. An aspect is the unit of build, load, and health. |
| `grip.rs` | Everything the orchestrator can do, in one `Arc<Grip>`. Role-aware: the gateway owns the database and the fleet; a worker (this process) owns the runtime and the checkout. |
| `ipc.rs` | The gateway↔worker wire: JSONL requests, responses and notes over an inherited socketpair. |
| `persist.rs` | How state is reached from either side: the gateway hits redb, a worker asks the gateway. |
| `host_api.rs` | The implementation of every host import. This is the whole surface a guest has against the system, so each function validates its arguments and caps its output. |

**Change and safety**

| File | Owns |
|---|---|
| `pipeline.rs` | The one path a change takes: build, validate, commit to the branch, cache the artifact, swap. |
| `builder.rs` | The serialized build queue. `cargo build --target wasm32-wasip2`, with a cross-process lock. |
| `devkit.rs` | The self-development operations, and path confinement. |
| `manifest.rs` | Structured edits to `[dependencies]` only, so the parts that make a crate a component survive. |
| `gitctl.rs` | Every git operation: commits, merges, worktrees, resets. |
| `branchops.rs` | Your branch verbs: status, log, update from trunk, reset, complete/abort merge. |
| `branches.rs` | The branch registry: which conversation runs which branch and checkout. |
| `merge.rs` | Gateway-side merging. The branch is squashed to one commit, then trunk fast-forwards; only a human triggers it. |
| `buildcache.rs` | Content-addressed artifacts plus smoke verdicts, keyed by tree oid. |
| `publish.rs` | The publish boundary: the filtered history that becomes `main` on the remote, and the pre-push guard. Local `main` is trunk and never leaves the machine. |
| `revisions.rs` | The retired revision registry, kept read-only for migration. |
| `watchdog.rs` | Liveness probes and the circuit breaker. Repeated traps reset an aspect's source to the branch's last green build. |
| `watcher.rs` | Hot reload. A human edit goes through the same pipeline as your own. |
| `control.rs` | Restarting this worker, optionally onto a kernel this branch built. |
| `workers.rs` | Gateway-side: spawning, supervising and reaping the worker fleet. |
| `branch_api.rs` | The `branch-*` frames the browser sends, handled host-side. |

**State, model and settings**

| File | Owns |
|---|---|
| `store.rs` | Sessions, event logs, the KV store, spend accounting, the branch registry. |
| `session.rs` | One tokio task for each active session. This makes concurrent input safe. |
| `llm.rs` | The chat-completions client, across every configured OpenAI-compatible provider. The request's `model` picks the endpoint via `Config::resolve_model`, and is rewritten to that provider's own name for it. Reassembles partial tool-call deltas, so you only see complete calls. |
| `cache.rs` | Prompt cache breakpoints. Applied host-side, so you cannot break caching by rewriting your loop. |
| `config.rs` | The three configuration layers and the `Secret` type. |
| `settings.rs` | Runtime reads and writes of the config file, through `toml_edit`, with the comments kept. |

**Skills, host services and the web**

| File | Owns |
|---|---|
| `skills.rs` | Parsing the corpus: frontmatter, the object model, the disclosure levels. |
| `skill_manager.rs` | The service layer: the discovery cache, per-session pinning, write confinement. It knows nothing about WIT. |
| `skill_index.rs` | Dense and BM25 ranking, parent absorption, child promotion. |
| `embeddings.rs` | Getting and caching vectors. Nothing here is load-bearing. |
| `hostfs.rs` | Host file access, confined to the configured roots. |
| `terminal.rs` | Long-lived shells. A unique marker after each command is what turns an endless stream back into request and response. |
| `web.rs` | The HTTP and WebSocket transport, and `/admin` in native code. |
| `gateway.rs` | Calls into the gateway component. The renderer keeps a warm instance because it renders one frame for each token. |
| `main.rs`, `lib.rs`, `roles/` | One binary, two roles: `thetis` is the gateway, `thetis worker` is a conversation's runtime. |

## The chat UI

Inside `gateways/gateway-web`, embedded in the component through `assets.rs`:

```text
ui/index.html      the shell
ui/theme.css       design tokens; the only file with colours
ui/app.css         layout and components
ui/app.js          entry point
ui/lib/socket.js   connection, reconnect, frame routing
ui/lib/store.js    client state with per-key subscriptions
ui/lib/dom.js      element helpers
ui/views/*.js      sidebar, transcript, composer, picker, panel
```

A new file needs one line in `assets.rs`. A new client action needs one function
in `handlers.rs` and one entry in its dispatch table.

### Seeing your own UI changes

The gateway component does two jobs, and only one of them is yours while you
work:

- **Rendering** — turning events into transcript frames — runs in *your*
  worker, from your build. Change it and your own conversation shows it at
  once.
- **Serving the interface** — the HTML, CSS and JavaScript a browser loads —
  runs in the gateway process, from **trunk's** build. Your version of those
  files reaches no browser until your work is merged.

So a UI edit compiles green and appears to do nothing. It is not broken and you
have not misunderstood it — you are simply not the one serving that file yet.

Open **`/preview/<your session id>/`** to see your own build. It serves your
interface against the real running system: the websocket, the workspace routes
and everything else stay live, so you are looking at your UI driving real
conversations rather than an empty copy. The page is served from the build
cache, so let the dev kit rebuild `gateway:web` first and then reload.

Do not start a second Thetis to look at a UI change. It was the only way once;
it is not any more.

## What the dev kit lets you write

Two different limits. Do not confuse them.

**Path confinement is absolute.** `devkit.rs` rejects any path that has `..` or a
drive prefix, joins the rest to `aspect_source_dir(aspect)`, and then confirms the
result is still inside that tree after symlinks are resolved. Therefore you can
never reach `wit/`, `crates/thetis`, or another aspect through `write_code` or
`patch_code`. Use the filesystem tools for those — they land in this branch's
checkout like everything else — then restart. Everything you write, however you
write it, stays on this conversation's branch until a human merges it.

**The protected lists are configurable and default to empty.** `protected_reason`
checks `devkit.protected_files` against the last path element and
`devkit.protected_dirs` against every element. Both are empty by default, so
`Cargo.toml` and `build.rs` in your own aspect **are** writable now.

Older documentation, including `README.md`, says that `Cargo.toml`, `build.rs`
and `.cargo/` are always off limits. That is out of date; the code once had a
hardcoded list and now reads the config. Confirm the current state with
`read_config devkit.protected_files` before you depend on either behaviour.

Prefer `add_dependency` and `remove_dependency` over a hand write of
`Cargo.toml`. They edit the `[dependencies]` table and nothing else. A manifest
also carries `[lib] crate-type = ["cdylib"]`, an empty `[workspace]` stanza, and
the `wit-bindgen` dependency. If one rewrite loses any of those, the aspect stops
being a component, which is worse than not compiling.

Remember also that a host-side build runs `build.rs` and any proc macro with the
privileges of the orchestrator.

## Failure branches

| Symptom | Action |
|---|---|
| A file is not where this map says | Run `list_code` for the aspect, or `list_path`. This map can be stale; the tools cannot. |
| A dev-kit write is refused | Read the message. It names the rule: path confinement, or which protected list matched. |
| You must change the kernel or the contract | The dev kit cannot reach them. Edit with `write_path`, then `restart_orchestrator` — it rebuilds the orchestrator for you in the background and reports here; a build that fails restarts nothing. Never run cargo on the kernel yourself. Only this conversation's runtime restarts, and a broken kernel falls back. For the contract, read `careful-surgery/contract-changes` first. |
