# Tier 3 snapshot

Taken 2026-08-20, at commit `ef281e8`.

**This is orientation only, not authority.** Each section gives the command that
makes it again. If a tool disagrees with this file, the tool is correct. Read
`references/volatility.md` for the rule.

## Configuration

Remake with: `list_config`

Values at the time of the snapshot, grouped. All are marked `[needs restart]`.

| Key | Value |
|---|---|
| `agent.default_mode` | `agent` |
| `agent.max_iterations` | 32 |
| `budgets.probe_secs` | 5 |
| `budgets.tool_secs` | 30 |
| `budgets.wasm_slice_secs` | 10 |
| `build.command` | `cargo` |
| `build.locked` | true |
| `build.profile` | `release` |
| `build.target` | `wasm32-wasip2` |
| `build.target_dir` | `target-wasm` |
| `build.timeout_secs` | 900 |
| `cache.anchor_stride` | 8 |
| `cache.enabled` | true |
| `cache.explicit_vendors` | `anthropic` |
| `cache.ttl` | `5m` |
| `context.compact_target` | 0.25 |
| `context.compact_threshold` | 0.6 |
| `context.enabled` | true |
| `context.keep_head` | 4 |
| `context.keep_tail` | 30 |
| `context.summary_model` | empty, so the main model |
| `context.window_tokens` | 200000 |
| `control.allow_restart` | true |
| `control.min_uptime_secs` | 20 |
| `devkit.enabled` | true |
| `filesystem.allow_delete` | true |
| `filesystem.enabled` | true |
| `filesystem.max_read_bytes` | 1048576 |
| `filesystem.protected` | `data, artifacts, .git` |
| `filesystem.roots` | empty, so the project root |
| `limits.agent_memory_mb` | 512 |
| `limits.gateway_memory_mb` | 128 |
| `limits.tool_memory_mb` | 128 |
| `limits.max_attachment_bytes` | 8388608 |
| `limits.max_attachments` | 8 |
| `limits.max_tool_output_bytes` | 32768 |
| `limits.session_spend_limit_usd` | 0, so no limit |
| `llm.base_url` | `https://openrouter.ai/api/v1` |
| `llm.max_retries` | 3 |
| `llm.model` | `anthropic/claude-opus-5` |
| `llm.request_timeout_secs` | 180 |
| `paths.agent` | `agents/agent-core` |
| `paths.artifacts` | `artifacts` |
| `paths.data` | `data` |
| `paths.gateway_prefix` | `gateway-` |
| `paths.gateways` | `gateways` |
| `paths.skills` | `skills` |
| `paths.templates` | `templates` |
| `paths.tool_prefix` | empty |
| `paths.tools` | `tools` |
| `paths.wit` | `wit` |
| `sandbox.enabled` | false |
| `server.admin_enabled` | true |
| `server.bind` | `127.0.0.1:7777` |
| `server.primary_gateway` | `web` |
| `skills.embedding_dimensions` | 1536 |
| `skills.embedding_model` | `openai/text-embedding-3-small` |
| `skills.max_query_chars` | 2000 |
| `skills.max_universal` | 20 |
| `skills.retrieval_enabled` | true |
| `skills.retrieve_limit` | 4 |
| `terminal.default_timeout_ms` | 30000 |
| `terminal.enabled` | true |
| `terminal.idle_timeout_secs` | 1800 |
| `terminal.max_output_bytes` | 65536 |
| `terminal.max_sessions` | 4 |
| `wasi.dirs` | `workspace` |
| `wasi.dns` | true |
| `wasi.env` | false |
| `wasi.network` | true |
| `wasi.stdio` | false |
| `watchdog.debounce_ms` | 500 |
| `watchdog.failure_threshold` | 3 |
| `watchdog.failure_window_secs` | 120 |
| `watchdog.probe_interval_secs` | 30 |
| `watchdog.watch_suppression_secs` | 5 |

`models` and `modes` are read-only lists. `tools.<name>.*` blocks are given to
that tool alone as its `config-json`.

## Modes

Remake with: `list_config modes`

| id | label | read_only |
|---|---|---|
| `agent` | Agent | false |
| `plan` | Plan | true, and it has a `prompt` |

## Models in the picker

Remake with: `list_config models`

`anthropic/claude-opus-5`, `anthropic/claude-sonnet-5`, `openai/gpt-4o`,
`google/gemini-2.5-pro`.

## Built-in tool groups

Remake with: read your own prompt, or `list_tools` through the Tools panel.

A group appears only when the capability behind it is available, so you are
never offered a tool that must fail.

| Group | Gate | Tools |
|---|---|---|
| Memory | always | `remember`, `recall` |
| Skills | always | `skill_fetch`, `skill_search`, `skill_write`, `skill_delete`, `skill_lint` |
| Sandbox | `sandbox::available()` | `exec`, `write_file`, `read_file` |
| Dev kit | `devkit_available` | `new_tool`, `write_code`, `patch_code`, `add_dependency`, `remove_dependency`, `list_dependencies`, `read_code`, `list_code`, `rollback`, `history` |
| Host files | `hostfs::available()` | `read_path`, `write_path`, `list_path`, `delete_path` |
| Terminal | `terminal::available()` | `terminal_open`, `terminal_run`, `terminal_read`, `terminal_send`, `terminal_signal`, `terminal_close`, `terminal_list` |
| SSH hosts | `terminal::available()` and `terminal::ssh_available()` | `ssh_host_list`, `ssh_host_get`, `ssh_host_set`, `ssh_host_remove`, `ssh_host_rename` |
| Configuration | always | `list_config`, `read_config`, `set_config` |
| Control | `control::available()` | `restart_orchestrator` |

Hot-loaded tool components are added on top, from `tooling::registry()`. They are
opaque, so a read-only mode does not offer them and treats an unknown name as
mutating.

## Host config keys the agent can read

Remake with: read `config_get` in `crates/thetis/src/host_api.rs`.

`config_get` is a small allow-list, not the whole configuration:

`model`, `agent_name`, `agent_avatar`, `system_prompt`, `max_iterations`,
`max_tool_output_bytes`, `sandbox_available`, `devkit_available`,
`compact_enabled`, `context_window`, `compact_threshold`, `compact_target`,
`summary_model`, `keep_head`, `keep_tail`.

`agent_name` is what the *agent* calls itself, from `agent.name` in the config,
defaulting to `Thetis`. It is not the harness's name: the harness is always
Thetis. `system_prompt` already has `{agent_name}` substituted by the host, so a
guest never has to do it.

`agent_avatar` is an image URL or `data:` URI from `agent.avatar`, empty when
none is set. The web gateway substitutes both into `index.html` at serve time
in `fill_identity`, which also decides which of the avatar `<img>` and the
built-in `<svg>` mark carries `hidden`.

Any other key returns nothing. If you need a new one in your loop, you must add
it to `host_api.rs` and restart, because that is kernel code.

## Session event variants

Remake with: read `variant session-event` in `wit/thetis.wit`.

`user-message`, `assistant-message`, `tool-invocation`, `tool-result`, `nudge`,
`system-note`, `modification`, `incident`, `turn-started`, `turn-finished`,
`stream-delta`, `context-compacted`.

`stream-delta` is the only transient case. It is rendered to clients and never
persisted.

## Host interfaces in the contract

Remake with: `grep '^interface\|^world' wit/thetis.wit`

`types`, `sys`, `session`, `skills`, `skills-view`, `llm`, `sandbox`, `tooling`,
`hostfs`, `terminal`, `control`, `configuration`, `devkit`.

Worlds: `agent`, `gateway`, `tool`.

The `agent` world imports every interface except `skills-view`. The `gateway`
world imports `sys`, `session` and `skills-view` only. The `tool` world imports
`sys` and `sandbox` only.

## Not yet built

Remake with: read the last section of `README.md`.

- The Docker exec sandbox. The capability is defined and wired, but the
  implementation is a stub. With `sandbox.enabled = false` you are simply not
  offered the tools, rather than given tools that fail.
- MCP. The imports exist and return empty. No client is connected.
- More gateways, and authentication for the web UI.
