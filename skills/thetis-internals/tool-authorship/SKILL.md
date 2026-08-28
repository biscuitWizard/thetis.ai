---
name = "Writing and maintaining tool components"
brief = "Build, edit, configure and debug a tool component: the tool world, describe/invoke, capabilities, config, filesystem and network reach, budgets and failure modes."
when_to_use = "Use when creating a new tool with new_tool, editing an existing tools/<name> crate, giving a tool an API key or other setting, adding a crate dependency to it, deciding what a tool is allowed to touch outside itself (files, HTTP, the KV store), deleting a tool, or debugging one that never appears in the tool list, returns truncated output, is withheld in a read-only mode, or crashes and gets rolled back. Not for adding a built-in tool to the agent's own loop beyond the two-edit summary here, and not for a user's own project code."
universal = false
tags = ["tools", "tool component", "new_tool", "describe", "invoke", "args schema", "capabilities", "read-only", "tool config", "api key", "wasm32-wasip2", "wit-bindgen", "hot reload", "filesystem", "workspace", "side effects"]
version = 2
---

# Writing and maintaining tool components

Every fact below was checked against the source, and the behavioural claims were
checked by running a probe tool. Citations are by symbol, not line number: this
tree gets renamed and reordered, and a symbol survives that.

**Naming note.** The project was renamed Genesis → Thetis. The kernel crate is
`crates/thetis`, the contract is `wit/thetis.wit`, config is `thetis.toml`, and
in kernel code the old `Harness` is `Grip` (`grip.rs`) and `Slot` is `Aspect`
(`aspect.rs`). Guest-side names did **not** change: the world is still `tool`,
and the bindings path is still `genesis::harness::*`. Do not "fix" that import.

## Two kinds of tool, and how to choose

| | Built-in | Component |
|---|---|---|
| Lives in | `agents/agent-core/src/tools.rs` | `tools/<name>/` |
| Reaches | everything the `agent` world imports | `sys`, `sandbox`, WASI, `wasi:http` |
| Changing it | rebuilds you; lands next turn | rebuilds that tool; callable next iteration |
| Read-only status | declared per tool | self-declared, default closed |

Choose a component unless the tool needs authority a tool is not given: a
terminal, the session log, the LLM, the dev kit, config writes, or host files
outside `workspace`.

To add a **built-in** instead: one `ToolDef` in the right group function and one
arm in `invoke`, both in `tools.rs`. Set `mutating` correctly — that is the only
thing a read-only mode filters on.

## The contract

`world tool`, at the end of `wit/thetis.wit`:

```wit
world tool {
  use types.{tool-manifest};
  import sys;
  import sandbox;
  export describe: func() -> tool-manifest;
  export invoke: func(session-id: string, args-json: string,
                      config-json: string) -> result<string, string>;
}
```

`tool-manifest` is four fields: `name`, `description`, `args-schema-json`,
`capabilities: list<string>`.

`sys` gives a tool `log`, `now-ms`, `kv-get`, `kv-put`, `config-get`,
`list-models`, `list-modes` — that is all of it.

- `kv` scope must be `"global"` or this call's own session id; anything else is
  refused by `scope_ok` in `host_api.rs`. Values cap at 1 MiB.
- `config-get` is a small allow-list of non-secret keys, not the config file.

## What a tool can affect outside itself

The WIT imports are not the whole story. **`build_linker` in `runtime.rs` calls
`wasmtime_wasi::p2::add_to_linker_async` and the `wasi:http` linker before the
`match caps`**, so every guest — tools included — gets full WASI preview 2 and
outbound HTTP. Only the `match` arm is capability-scoped, and for `Caps::Tool`
it adds just `sandbox`.

So a tool has four routes to the outside world. Document all four in the tool's
own description when it uses them; a caller cannot see them from the schema.

### 1. The filesystem — real, and wider than it looks

Tools **do** have filesystem access. `wasi_ctx()` is shared by every guest and
preopens each `wasi.dirs` entry with `DirPerms::all()` and `FilePerms::all()` —
read *and* write. Default config is `wasi.dirs = ["workspace"]`.

Measured from inside a tool, not inferred:

| Attempt | Result |
|---|---|
| `current_dir()` | `/` |
| `read_dir("/workspace")` | ok — lists real project content |
| `write("/workspace/x.txt")` | ok |
| `read_to_string("/workspace/x.txt")` | ok, survives a restart |
| `read_to_string("/workspace/../thetis.toml")` | `Operation not permitted (os error 63)` |
| `read_to_string("/etc/passwd")` | `No such file or directory (os error 44)` |
| `read_to_string("thetis.toml")` (relative) | `No such file or directory (os error 44)` |

Both halves matter:

- **The grant is genuine.** A tool can persist state, read files a user put
  there, and leave files behind. Writes outlive the call and the process.
- **The confinement holds.** A preopen is a capability, not a path prefix, so
  `..` cannot climb out of it and nothing outside the preopens is nameable. The
  kernel, the contract and `thetis.toml` are unreachable this way.
- **`workspace/` is shared by every branch and conversation**, unlike
  `worktrees/`. A tool writing there is visible to every other conversation, so
  it is a side-effect channel between them, not scratch space. Namespace files
  you create, and do not assume you are alone.

There is no per-tool filesystem scoping: every tool gets the same preopens. If
one tool should not touch files, that is a property of its code, not something
the host enforces. To take filesystem access away from all tools, set
`wasi.dirs = []` — that costs the agent's guest access too, and needs a restart.

### 2. Network

`wasi:http` is linked for every guest, and `wasi.network` and `wasi.dns` are on
by default. TLS is terminated host-side, because no TLS crate builds for
`wasm32-wasip2` — `ring` and `openssl` both want a C toolchain targeting wasm.
Use `waki` in the guest. A tool can therefore reach any host the machine can,
which is the widest authority a tool has: say so in the description, and name
the service.

### 3. The KV store

`kv-put` persists across calls and restarts. Scope `"global"` is visible to
every session and every tool; a session id is not. Prefer the session scope
unless the data is genuinely shared.

### 4. Whatever the remote API does

An HTTP call that creates a Notion page or a GitHub commit is a side effect the
sandbox cannot see or undo. This is what `capabilities` is for.

## Creating one

```
new_tool(name, description)
```

`devkit::new_tool`:

1. Validates the name — `validate_component_name` in `aspect.rs`: 1–48 chars,
   `[a-z0-9-]` only, no leading or trailing hyphen. Uppercase and `_` rejected.
2. Refuses if `tools/<name>/` exists — edit it with `write_code` instead.
3. Renders `templates/tool-template/{Cargo.toml,lib.rs}.template`, substituting
   `{{name}}` and `{{description}}`; the description is escaped and flattened to
   one line because it lands in Rust string literals.
4. Runs the full pipeline, so the tool is **built, validated, smoke-tested and
   loaded before the call returns**. No restart.

`paths.tool_prefix` is empty, so the directory name *is* the aspect name *is*
the tool name the model sees.

## What the template gives you

```rust
wit_bindgen::generate!({ world: "tool", path: "../../wit", generate_all });

use genesis::harness::sys;
use genesis::harness::types::LogLevel;

struct Component;
impl Guest for Component {
    fn describe() -> ToolManifest { /* name, description, args_schema_json, capabilities */ }
    fn invoke(session_id: String, args_json: String, config_json: String)
        -> Result<String, String> { /* ... */ }
}
export!(Component);
```

`ToolManifest` and `Guest` come from the world's own `use types.{...}`; anything
else must be imported from `genesis::harness::types`.

`Cargo.toml` carries four things that must survive every edit: an empty
`[workspace]` (so the crate is standalone), `[lib] crate-type = ["cdylib"]`,
`wit-bindgen`, and `[profile.release] opt-level = "s"`. Lose one and the crate
stops being a component — worse than not compiling.

## Rules `describe` must satisfy

`smoke_test` in `pipeline.rs` rejects a build that breaks any of these, and the
old revision keeps serving:

1. `name` non-empty.
2. `name` equal to the aspect name. A mismatch would make the tool uncallable —
   the model told one name, the registry keyed by another.
3. `args_schema_json` parses as JSON.

Beyond the gate, make the schema a JSON Schema **object**: it becomes
`function.parameters` in the model's tool definition.

Never name a component after a built-in. Dispatch matches built-in arms first
and only falls through to `tooling::invoke` on `other`, so the component is
permanently shadowed while still appearing in the tool list.

## `capabilities` — one string is load-bearing

`READ_ONLY_CAP = "read-only"` in `agents/agent-core/src/tools.rs` is the only
value the system interprets.

- A hot-loaded tool is opaque, so a read-only mode withholds it **unless** it
  declares `"read-only"` — enforced in four places: the model's tool list, the
  UI panel, the manifest view, and dispatch itself.
- Default is closed: a tool that says nothing is treated as mutating.
- It is the tool's assertion about itself, trusted no further than the tool.

Every other string is documentation for a human. Convention here: read-only web
tools declare `["http", "read-only"]`, writers declare `["http"]`. A tool that
writes files or the global KV scope is **not** read-only, however much it feels
like a reader — declare accordingly.

Getting this wrong hands a read-only mode a tool that can delete things.

## Per-tool configuration

`invoke`'s third argument is this tool's own `[tools.<name>]` block as JSON, or
`{}`. A tool never sees another's settings, or anything else in the config
(`Config::tool_config_json`).

1. **Group inheritance.** A hyphenated name reads every prefix, least specific
   first: `notion-search` merges `[tools.notion]` then `[tools.notion-search]`,
   more specific winning key by key (`tool_config_scopes`). A family shares one
   credential block.
2. **Environment wins over the file**, per scope (`tool_env_overlay`).
   `<SCOPE>_API_KEY`, `_TOKEN`, `_API_TOKEN`, `_ACCESS_TOKEN` all land as
   `token`, and a `<PREFIX>_TOOL_<SCOPE>_<KEY>` form sets any key. Hyphens in
   the scope become underscores. Read `tool_env_overlay` for the exact prefix
   before relying on it — it changed with the rename.
3. **`*_path` keys are read for you.** The host reads the file and inlines it as
   `*_contents`, confined to `secret_roots()`; a failure arrives as
   `*_contents_error` rather than an exception, so the tool can explain it
   (`inline_file_secrets`).

Secrets belong in the gitignored local config, not the committed `thetis.toml`.
Config is read at startup: **a config change needs `restart_orchestrator`.**

Verify what a tool actually receives with `config-probe`, which returns its own
block:

```
input: validate config delivery
my settings: { "greeting": "hello from configuration", "retries": 3 }
```

## Dependencies

Prefer `add_dependency` / `remove_dependency`: they edit `[dependencies]`
through `toml_edit` and leave everything else byte-identical, comments included
(`manifest.rs`). Registry deps only — no `git` or `path` sources.
`build.allowed_crates` is empty, so any crate is permitted.

A dependency change rebuilds without `--locked`, and the manifest is restored if
the build fails.

The hard constraint is the `wasm32-wasip2` target. Pure-computation crates
almost always work; anything wanting a C toolchain or its own TLS does not.

## Limits a tool runs under

| Limit | Default | Config key |
|---|---|---|
| Memory | 128 MB | `limits.tool_memory_mb` |
| Instances / tables | 8 / 64 | — (hardcoded in `runtime.rs`) |
| Time without yielding to a host call | 30 s | `budgets.tool_secs` |
| `describe` probe | 5 s | `budgets.probe_secs` |
| Output, ok **or** err | 32768 bytes | `limits.max_tool_output_bytes` |
| Preopened dirs | `workspace` | `wasi.dirs` |
| Host env vars | off | `wasi.env` |

There is deliberately no wall-clock ceiling. The budget measures time spent
*not* talking to the host, because that distinguishes a wedged guest from a slow
one; a guest that is merely computing gets `UpdateDeadline::Yield` so it cannot
hold a runtime thread solid. Output past the cap is cut at a char boundary and
annotated `[truncated: N of M bytes shown]` (`Grip::truncate`) — so paginate or
summarise inside the tool rather than relying on the cut.

Confirm current values with `list_config`; the numbers above are defaults.

## The edit loop

Any `write_code` / `patch_code` on `tool:<name>` runs the whole pipeline and
returns the compiler's verdict in the same tool result: compile →
identical-artifact check → wasmtime validation → smoke test → commit to this
branch → cache the artifact → swap.

- A failed gate changes nothing that is serving. The report names the gate.
- Unlike a change to yourself, a tool change is live immediately: `pending_swap`
  is set only for `Aspect::Agent`.
- Writes are confined to `tools/<name>/`: no `..`, no absolute path, re-checked
  after symlinks (`devkit.rs`). `devkit.protected_files` and `protected_dirs`
  are empty by default, so `Cargo.toml` **is** writable.
- Editing under `tools/` with `write_path` or a shell works too: the watcher
  pushes it through the same pipeline. A change under `wit/` rebuilds every
  guest.
- A terminal `cargo build` produces an artifact but does **not** load it. Only
  the pipeline swaps. A comment-only `patch_code` forces a reload.

Building by hand, from inside the crate (each tool is its own package, so `-p`
from `tools/` does not work):

```bash
cd tools/<name>
cargo build --release --target wasm32-wasip2 --target-dir ../../target-wasm
```

## Deleting a tool

Delete the directory. The watcher notices, and the pipeline's first step sees a
missing `Cargo.toml` for a tool aspect and takes it out of service —
`Grip::uninstall_component` drops the manifest and then the loaded component,
committing `removed tool/<name>`. The next call returns
`no tool named '<name>' is loaded`.

Order matters inside that function: the manifest goes first, because
`tool_registry` filters the manifest map by what the loader holds, so a reader
caught between the two writes must see a tool that is absent rather than one
loaded but undescribed.

This only applies to tools. For the agent or a gateway a missing crate is a
fault to report, not an instruction to unload the system's own moving parts.

## Failure branches

| Symptom | Cause | What to do |
|---|---|---|
| Tool absent from your tool list | `describe` failed after load, so no manifest was recorded — or the mode is read-only and it lacks `"read-only"` | Re-run a trivial `patch_code` and read the report |
| Build refused: `tool manifest says 'x' but the aspect is 'y'` | `describe`'s name drifted from the directory name | Make them equal |
| Build refused: `argument schema is not valid JSON` | `args_schema_json` is not JSON | Build it with `json!({...}).to_string()` |
| `tool '<name>' crashed: ...` | The component trapped. Reported as a tool result, not a trap, so the conversation survives | Fix it. Repeated failures in the watchdog window trip the breaker and reset the aspect to its last green build |
| Refused: "changes things, and this conversation is in \<mode\> mode" | Read-only enforced at dispatch, not just in the list | Switch modes, or declare `"read-only"` if it truly only reads |
| Result ends `[truncated: ...]` | Output exceeded the cap | Trim inside the tool |
| A setting you added is not visible | Config is read at startup | `restart_orchestrator` |
| Build times out or blocks for a very long time | `build.timeout_secs` is 900, but every worktree builds into its own `target/` and shares one cargo lock. Several conversations building at once serialise, and `Blocking waiting for file lock on build directory` can last tens of minutes | Check `pgrep -af 'cargo (build\|test)'` before concluding anything is wedged. Detach a long kernel build with `setsid nohup ... > /tmp/log 2>&1 &` so a restart does not kill it, and poll the log |
| A tool reads a different setting than `thetis.toml` says, or a config test fails only on a machine with credentials | The environment beats the file, per scope. A real `NOTION_TOKEN` overrides `[tools.notion] token` everywhere, tests included | Working as designed. Assert on a fictional `zz*` scope in tests, and check the environment before disbelieving the file |
| A file written to `workspace/` appeared in another conversation | `workspace/` is shared across branches by design | Namespace your files; do not use it as private scratch |
| Deleted the directory but the tool still answers | The running kernel predates the deregistration step | Restart; aspects are rediscovered from disk at startup |
| A skill or path under the old `genesis-*` name is missing | The rename moved the corpus to `thetis-internals` and the kernel to `crates/thetis` | Use the new ids; check `ls skills/` rather than guessing |
