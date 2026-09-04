---
name = "Configuration, restart and recovery"
brief = "The three config layers, which settings need a restart, modes, models and providers, prompt caching, and the safety nets that make a bad change recoverable."
when_to_use = "Use when you must change a setting, add a mode/model/provider, understand why a change did not take effect, restart this conversation's runtime, or recover from a bad change. Not for the step-by-step procedure of editing code, which is careful-surgery."
universal = false
tags = ["config", "thetis.toml", "restart", "rollback", "branch", "watchdog", "recovery", "modes", "models", "providers", "openrouter", "llama.cpp", "vllm", "local model", "prompt cache", "admin", "validation gate", "epoch watchdog", "circuit breaker", "branch resets", "tool-group:config", "tool-group:branch", "tool-group:selfmod"]
related = ["careful-surgery"]
version = 5
---

# Configuration, restart and recovery

## The three layers

Each layer overrides the one before it:

1. The defaults compiled into `config.rs`. Thetis runs with no config file.
2. `thetis.toml`, or the file that `THETIS_CONFIG` points at.
3. Environment variables, for per-run overrides and secrets.

A key that this build does not recognise is reported and ignored, not fatal. The
reason is important: you edit the config file and the code that reads it, and
the binary that understands a new section is built by a process that runs only
while the service is up. A config that is one step in front of its binary must
not be able to hold the service down.

A bad **value**, such as a `default_mode` that is not among the configured modes,
does fail at startup and names the key.

## Reading and writing settings

| Tool | Effect |
|---|---|
| `list_config(prefix?)` | Every setting as a dotted path with its value. |
| `read_config(key)` | One setting. |
| `set_config(key, value)` | Write it back to the file. |

The type of a new value is taken from the existing value.

Two guards on a write. **Validation**: the candidate text goes through the whole
load path before it replaces anything. Thetis refuses to start on a bad config,
so writing one and then restarting is the one failure that has no in-band
recovery. **Redaction**: a secret can be written but never read back.

`toml_edit` does the write, so the comments that explain each setting survive.

**Most settings apply at once.** A write reloads the configuration in the
process that made it: everything read at use — the model and providers, the
prompt, limits, budgets, context, tool groups, file and shell access, the
accounts and roles — is in force from the next call. The few that something
was built from at boot (the listener, memory ceilings, the WASI sandbox, the
watchdog, the browser and Discord connectors, paths) are marked
`[needs restart]`, and the reply to `set_config` names them; call
`restart_orchestrator` to finish those. A worker reloads its own worktree's
`thetis.toml` and the shared `thetis.local.toml`; the gateway and other
workers read theirs, so an installation-wide change is best made from the
control panel, which reloads the gateway and every live worker.

The sections are documented in `thetis.toml` itself. Read the file rather than a
list here. `references/volatility.md` says why.

## Modes and models

A **mode** is how a conversation works. `agent` is the default. `plan` is
read-only: every mutating tool is withheld, and also refused at dispatch, so a
model that remembers a tool from earlier in the conversation still cannot call
it.

Adding a mode is configuration alone: one `[[modes]]` block with `id`, `label`,
`description`, `read_only`, and an optional `prompt`. The agent never knows any
mode by name. It asks `list_modes` for `read_only`, and appends `prompt` to the
system prompt.

Withholding tools tells the model what it cannot do but never what it should do
instead. That is what the `prompt` field is for.

A **model** is a per-conversation override, chosen from `[[models]]` or
`THETIS_MODELS`. Empty means the grip default.

## Providers

Thetis speaks to any number of OpenAI-compatible endpoints: OpenRouter, a local
llama.cpp or vLLM server, a company gateway. `[llm]` is *always* registered as a
provider under the id `openrouter`, so `[[providers]]` is purely additive and a
config that lists none behaves as it always did. An entry whose id **is**
`openrouter` replaces that synthesized one rather than sitting unreachable
behind it.

`Config::resolve_model` decides which endpoint serves a request, in this order:

1. a matching `[[models]]` entry that names a `provider`;
2. a provider-id prefix on the model id — `local/qwen3` reaches the provider
   called `local` asking for `qwen3`, with no `[[models]]` entry needed;
3. `llm.provider`, defaulting to `openrouter`.

A prefix that is not a configured provider id is left alone, so an OpenRouter id
like `anthropic/claude-opus-5` is never mistaken for routing. Do not give a
provider the same id as an OpenRouter vendor.

`wire_model` on a `[[models]]` entry separates the id Thetis uses everywhere —
picker, session record, `THETIS_MODEL` — from the name the endpoint knows the
model by. A local server usually wants a bare name where the picker wants
something namespaced.

### Scaling a provider

A provider takes either `base_url` (one endpoint) or `base_urls` (several
interchangeable ones, normalized to the same list internally — `base_url()`
returns the first). Requests rotate over the list via a process-wide counter,
and **each retry advances to the next endpoint**, so a dead or overloaded
replica is stepped over rather than retried in place.

The entries must serve the same model: this is replication, not model routing.
Scaling this way leaves every model id unchanged, which is the point — capacity
is not a picker concern. Note that one `llama-server --parallel N` already
serves N concurrent slots on a single port, so `base_urls` is for separate
processes or machines.

Two behaviours worth knowing:

- **No key means no header.** A provider with no `api_key` sends no
  `Authorization` at all, because an empty bearer token is rejected outright by
  some servers. Only an OpenRouter-hosted provider fails fast on a missing key.
  `api_key = "env:NAME"` reads it from the environment, and an unset variable
  leaves the provider unauthenticated rather than failing.
- **Errors name the provider.** A provider error detail is prefixed `[<id>]`,
  because "404 model not found" reads very differently against a local server
  than against OpenRouter.

Embeddings route the same way, either by the id in `skills.embedding_model` or
by naming `skills.embedding_provider` outright.

### Timeouts are about silence, not length

`llm.request_timeout_secs` is a **read** timeout for a streaming completion: it
resets whenever bytes arrive, so a slow local model may generate for as long as
it likes provided it keeps sending, and only a genuine stall trips it. For a
non-streaming call it is the total deadline for the whole request.

Do not "simplify" this back to reqwest's `ClientBuilder::timeout`. That is a
deadline on the whole request *including the body*, which for a stream is a cap
on total generation time — it severs a long answer mid-body, and the user sees
`transport error: error decoding response body`. A test in `llm.rs` drives a
real socket that trickles past the timeout and fails if this regresses.

### A broken stream keeps what arrived

Once response headers are in, a mid-body failure is not treated as a failed
turn. `SsePump::abort` checks whether anything usable arrived — answer text
already shown, or accumulated tool calls — and if so closes the stream as
`Finished` with reason `error` rather than as `Err`. The user keeps the partial
answer, completed tool calls still run, and the transcript stays true.

Tool calls whose accumulated arguments are not valid JSON on their own are
dropped, because dispatching half-parsed arguments is worse than dropping the
call. Reasoning does not count as salvageable: it is never persisted, so a break
during the thinking phase still surfaces as an error. With nothing to salvage
the error propagates unchanged, since hiding it behind an empty `Finished` would
turn a failure into a silent no-op.

A model or `llm.provider` naming a provider that does not exist is rejected at
startup — otherwise the mistake surfaces as a confusing 404 mid-conversation.

**A local model must support tool calling.** Thetis is useless without it.
llama.cpp's server needs `--jinja` or it does not apply the model's chat
template and no tool call is ever produced.

## Prompt caching

This is the largest cost lever. A repeat turn in a long conversation costs about
one tenth of a fresh one.

The strategy is per vendor, because providers differ in kind:

| Vendor | Behaviour | What Thetis does |
|---|---|---|
| Anthropic | Caches nothing unless the request marks where | Writes explicit `cache_control` breakpoints |
| OpenAI | Caches long prefixes automatically | Nothing. A mark would only bill writes. |
| Google | Implicit on recent models; an explicit mark bills a full-price write, and only the last mark counts | Left implicit |

The Anthropic cache is a prefix cache over `tools`, then `system`, then
`messages`. A breakpoint writes one entry that covers everything up to that
block. A later request hashes its prefix at each breakpoint and walks back **at
most twenty blocks** to look for a match.

That window is the whole reason for the design. A turn that runs a dozen tools
can add more than twenty blocks at once, so one breakpoint at the end would go
past the previous entry and read the whole conversation again at full price.
Thetis therefore places up to four breakpoints, which is the Anthropic limit:
on the last system message, on two **anchors** that sit at a fixed stride and so
hold still while the conversation grows around them, and on the final message.

`cache.rs` applies the breakpoints host-side, after the model is resolved, and
whatever the agent sent. You cannot break caching by rewriting your own loop.
You **can** break it by making the system prompt unstable. See the
`turn-lifecycle` child.

## Restarting

`restart_orchestrator(reason, resume?)` restarts **this conversation's own
runtime** — no other conversation notices. It is needed for a change to a
startup-only setting, or to run a kernel you rebuilt in this branch (build it
first — `restart_orchestrator` does that for you).

- The restart is scheduled, not immediate. The call returns, the turn finishes,
  and the user reads the reason before the process goes away. Say why first.
- A rebuilt kernel is probed before it is adopted; one that cannot answer, or
  that stops speaking the gateway's protocol, or that crash-loops at startup,
  is set aside and the conversation continues on the previous kernel. The
  branch source stays untouched either way.
- `resume` defaults to true and the turn continues afterwards. Set it false only
  when the restart is the last thing you mean to do.

## The four safety nets

No self-modification can make the system unreachable — and nothing you change
in this conversation can break any other, because every conversation runs on
its own branch of the source tree in its own process.

1. **The validation gate.** A candidate must compile, load as a component for
   its world, and pass a smoke test — the agent answers a health probe, a
   gateway serves its index page, a tool returns a valid manifest — before it
   goes live. Only then is the source committed to the branch, so the last
   commit is always a green point; a failed build leaves the tree dirty for
   you to iterate on. The running build is untouched either way.
2. **The epoch watchdog.** Each guest call has a limit on how long it may run
   without a return to a blocking host import. There is deliberately no
   wall-clock ceiling: a turn that streams a long answer and compiles something
   is doing its job. What makes a wedged guest different is that it stops
   talking to the host, and that is what the slice measures. An infinite loop
   traps in seconds and the process is unharmed.
3. **The circuit breaker.** Repeated traps from one component reset its source
   to this branch's most recent green build automatically — as a new commit,
   nothing rewritten — and the incident is written into the conversation.
4. **The branch itself.** Every green build, skill edit, and end of turn is a
   commit; `branch_log` is the history and `reset_branch` restores any point.
   Built artifacts live in a content-addressed cache keyed by the source tree,
   so a reset reloads instantly instead of rebuilding.

`/admin` is rendered by the orchestrator in native code with no WebAssembly in
its path. It shows trunk's history and every conversation's branch with
stop/abort/reset controls, and it keeps working when every guest and every
worker is broken. The same controls, plus every setting, are in the web UI's
control panel (sidebar footer, administrators only), which the gateway guest
reaches through the `admin` host interface; both draw on `admin.rs` and
`settings/`, so a control exists once. Configuration writes from either the
agent's `set_config` or the panel go to `thetis.local.toml` for secrets,
accounts and keys already set there, and to `thetis.toml` otherwise; both files
are validated together before anything is written.

## Recovery procedure

1. `branch_status` and `branch_log` — see where the branch stands and what its
   history holds.
2. `reset_branch rev=<commit>` — restore the whole tree to that commit, as a
   new commit.
3. If two attempts have not fixed a regression, reset and start again from a
   green commit. Do not add more patches.
4. If every guest is broken, the human uses `/admin`.

## Trunk, merging, and conflicts

Your branch forks from trunk when the conversation starts. `update_from_trunk`
pulls in what has landed since (a fast-forward when possible); merging your
branch **to** trunk is a human action in the UI — you cannot do it, and should
say so if asked. A conflicted update leaves standard git conflict markers in
the working tree and lists the files; resolve them with the ordinary editing
tools and call `complete_merge`, or `abort_merge` to give up.

A merge to trunk is **squashed**. Everything the branch has of its own becomes
one commit parented on trunk, titled after the conversation, whose body lists
the subjects it absorbed; trunk then fast-forwards to it. So trunk's log carries
one line per conversation, not one per checkpoint. The squash reuses the branch
tip's tree unchanged, so no file moves, the worktree stays clean and cached
builds stay valid — but the branch's own commit ids are rewritten, and `rev`
values you noted from `branch_log` before a merge no longer name commits on the
branch. The pre-squash tip is kept at `refs/thetis/presquash/<branch>/<ms>` if
the detailed history is ever needed.

Directories
holding a `.thetis-private` marker never leave this machine: they stay fully
usable locally, and the publish machinery filters them from anything pushed.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A setting change did nothing | Configuration is read at startup | `restart_orchestrator`. |
| `transport error: error decoding response body` | A stream was cut off mid-body. Usually the timeout shape; otherwise the server or a proxy dropped the connection. | Check `llm.rs` still uses `read_timeout`, not `timeout`. The message now carries reqwest's source chain, so read past the first clause. |
| A long answer stops part-way and the turn ends with reason `error` | A mid-stream break that was salvaged rather than failed | Expected behaviour, not a bug. The endpoint dropped the connection; check the server's own log. |
| `set_config` was refused | The result would not load | Read the message. Fix the value. |
| A build succeeded but the aspect still traps | The failure is at instantiation, not at compile time. Usually a contract mismatch. | `reset_branch`, then read `careful-surgery/contract-changes`. |
| A component reset by itself | The circuit breaker fired | Read the incident in the conversation. Find the trap before you try the change again. |
| An update from trunk stopped | Conflicts | Resolve the markers, `complete_merge`; or `abort_merge`. |
