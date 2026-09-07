---
name = "The mooR MCP host"
brief = "What moor-mcp-host exposes to an AI assistant, how it authenticates as an ordinary MOO player, and where its safety boundary actually sits."
when_to_use = "Use when connecting an AI assistant to a running MOO, or auditing what it can reach. Use it when a tool returns a permission error or the connection goes stale. Not for the RPC layer it sits on (read daemon-and-rpc), and not for what a MOO permission actually allows (read moor/execution/permissions-and-security) or out-of-process workers (read workers)."
universal = false
tags = ["moor", "mcp", "model context protocol", "ai assistant", "moor-mcp-host", "moor-web-mcp", "stdio", "wizard", "programmer", "moo_*", "resources", "prompts", "credentials", "tools", "safety"]
version = 2
---

# The mooR MCP host

`moor-mcp-host` is a Model Context Protocol server that gives an AI assistant a
structured way to explore and edit a running MOO. It is a client of the daemon,
not a part of it. Everything it can do, a person with the same credentials could
do from a MOO prompt.

## What it is

A binary that speaks JSON-RPC 2.0 over stdio. The assistant's client spawns it;
there is no listening socket. On the other side it holds ordinary daemon RPC
connections, made with the same `RpcClientArgs` as any host, and logs in with a
username and password like any player.

It exposes three MCP surfaces:

| Surface | Contents |
|---|---|
| Tools | Around fifty `moo_*` operations: evaluate, run a command, invoke a verb, list and read and program verbs, list and read and write properties, create, recycle, move and reparent objects, dump and load objdef, inspect connected players, queued tasks and server info |
| Resources | Read-only URIs for a world overview, object listings, one object, one verb, one property, and system information |
| Prompts | Static explainers for the MOO language, the object model and the permission model |

The authoritative tool list is the vector built in `crates/mcp-host/src/tools/mod.rs`
and its dispatch table below it. Do not copy a list of tool names into
documentation; read it there.

## Two connections, two privilege levels

The host manages up to two logins, created lazily on first use:

- **Programmer**, from `--username` and `--password`. The default for every tool.
- **Wizard**, from `--wizard-username` and `--wizard-password`. Optional.

Each tool is registered one of two ways. Most take an optional `wizard`
parameter and run as the programmer unless it is set. A few are wizard-only:
every objdef operation, the object diff, and the command-verb dispatch tool. If
wizard credentials are not configured, a request for wizard falls back to the
programmer connection, which then simply fails on permissions inside the MOO.

Connections are health-checked and reconnect with backoff when they go stale.

## Where the safety boundary actually is

This is the part to be precise about, because it is easy to assume more
containment than exists.

**Inside the MOO, the boundary is the MOO's own permission system.** The MCP host
holds no special authority in the daemon. It presents an `AuthToken` like any
client, and every verb it runs is checked by the VM against the player it logged
in as. Read `moor/execution/permissions-and-security` for what that check
covers. If you want to limit an assistant, limit the character you give it: a
programmer bit is a very different grant from a wizard bit.

**Outside the MOO, there is no boundary.** Four objdef tools read and write files
on the machine at paths the caller supplies, using ordinary filesystem calls with
no sandbox and no root directory. An assistant driving this host can read any
file the process can read and overwrite any file it can write. Compare the file
worker, which exists precisely so that MOO code cannot do this and which confines
every path to one directory capability. The MCP host does not do that.

The consequences:

1. Run it as an OS user with nothing else to lose, in a working directory you are
   willing to have rewritten. Do not run it as the daemon's user.
2. Credentials arrive on the command line or in a YAML config file. Command-line
   passwords are visible to any process listing on the machine. Prefer the config
   file, and give it restrictive permissions.
3. Anything that can spawn the binary inherits its credentials. There is no
   second factor and no per-tool confirmation.
4. If CURVE setup fails, the host logs a warning and **continues without
   encryption** rather than refusing. Over a `tcp://` endpoint that means a
   failed enrollment degrades to a connection that will be refused by the
   daemon's ZAP handler; do not read the warning as harmless.
5. Give it a wizard login only when you need one. Wizard-only tools fail cleanly
   without it.

## The other MCP path

`clients/moor-web-mcp` is a separate, TypeScript, stdio MCP server that goes
through the **web host's** HTTP API instead of the daemon's ZeroMQ RPC. It takes
a JSON config listing one or more MOOs and, for each, named characters with their
credentials.

Prefer it when the assistant should be limited to what a web client can do, or
when the daemon's RPC socket is not reachable. Prefer `moor-mcp-host` when you
need the operations the web API does not expose. Neither gives an assistant
authority the character it logs in as does not have; only `moor-mcp-host` gives
it local filesystem access.

## Invariants

1. **The MCP host is a client.** It never bypasses login, tokens, sessions or
   permissions. If a change would let it do something a player could not, the
   change is wrong.
2. **Privilege comes from the character, not from the tool.** A tool marked
   wizard-only is a convenience so it fails early; it is not the security
   control.
3. **A new tool must state which connection it uses.** Register it with the
   optional wizard parameter or as wizard-required. There is no third option.
4. **Logging goes to stderr.** stdout is the MCP channel; anything written there
   corrupts the protocol.
5. **Tool arguments are untrusted input from a model.** Validate them. The
   existing filesystem tools do not, and that is the gap described above, not a
   pattern to copy.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/mcp-host/src/main.rs` | Arguments, config file, CURVE setup, credential assembly |
| `crates/mcp-host/src/mcp_server.rs` | The stdio JSON-RPC loop and the reconnect meta-tool |
| `crates/mcp-host/src/connection.rs` | Lazy programmer and wizard connections, health checks, reconnect |
| `crates/mcp-host/src/moor_client.rs` | The daemon client: login, eval, command, verb and property operations |
| `crates/mcp-host/src/tools/` | The tool definitions and their dispatch; `objdef.rs` holds the filesystem tools |
| `crates/mcp-host/src/resources/`, `prompts.rs` | Read-only resources and static prompts |
| `clients/moor-web-mcp/` | The web-host-backed alternative |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Every tool returns a permission error | The configured character lacks the bit | Give the tool `wizard: true` if wizard credentials exist, or use a character with the right permissions. Do not raise the daemon's privileges |
| A wizard-only tool fails as if unprivileged | No wizard credentials configured, so it silently used the programmer connection | Configure the wizard login, or accept that the tool is unavailable |
| The host starts and no tool works | Login failed | Check the username and password against a normal client first; the MCP host has no special login path |
| "Failed to setup CURVE auth (will try without encryption)" | Enrollment failed | Fix enrollment before trusting the connection. Over TCP the daemon will refuse it anyway |
| The MCP client reports protocol corruption | Something wrote to stdout | All logging must go to stderr |
| An objdef file tool wrote somewhere unexpected | The path came from the model and nothing constrained it | This is the known gap. Constrain it at the OS level: a dedicated user, a dedicated working directory |
| Tools stop responding after idle | The daemon connection went stale | The connection manager reconnects with backoff; the reconnect tool forces it |

## Read first / read next

- Read `moor/execution/permissions-and-security` before deciding which character
  an assistant gets. That decision is the whole security posture.
- Read `daemon-and-rpc` for enrollment, CURVE and tokens, which the MCP host
  uses unchanged.
- Read `workers` for the contrast: how mooR confines a filesystem capability when
  it is designed to be confined.
