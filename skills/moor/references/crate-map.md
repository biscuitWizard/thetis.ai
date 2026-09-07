# The mooR crate map

Every crate in the workspace, what it owns, and which layer it sits in. Fetch this
when you must find the crate that owns a behaviour, or when you must decide where a
new piece of code belongs.

The workspace root `Cargo.toml` is the authority for the member list and for every
dependency version. If a crate here is missing from that file, this map is stale.

## The layers

Dependencies point down this table and never up. A crate may use anything below it.
A cycle is a design error, not a build problem to work around.

| Layer | Crates | Owns |
|---|---|---|
| 0 — values | `moor-var` | The MOO value types themselves, and the compiled program representation. Depends on no other crate in the workspace. |
| 0 — macros | `moor-schema-macros`, `moor-builtin-docs-macro` | Procedural macros. One removes FlatBuffers conversion boilerplate; one extracts builtin-function documentation at compile time. |
| 1 — model | `moor-common` | The entities every other crate shares: objects, properties, verbs, permissions, sessions, task events, command matching, and utility code. |
| 1 — encoding | `moor-schema` | The FlatBuffers entities used for both RPC and persistence, and the conversions to and from the in-memory types. |
| 2 — language | `moor-compiler` | Lexing, parsing, code generation, decompilation, and unparsing. Also the object-definition literal syntax. |
| 2 — storage | `moor-db` | The transactional world-state store: relations, indexes, caches, the commit pipeline, and the storage provider. |
| 2 — protocol | `moor-runtime-api` | The typed message vocabulary between daemon, hosts, workers, and clients, and its wire codec. |
| 3 — execution | `moor-vm` | Frames, activations, execution state, and the bytecode execution loop. |
| 3 — transport | `moor-zmq-client` | The ZeroMQ implementation of the runtime client: what a host or a worker uses to reach the daemon. |
| 3 — content | `moor-textdump`, `moor-objdef` | The two database source formats. `moor-textdump` reads and writes LambdaMOO textdumps. `moor-objdef` reads and writes the directory-based object-definition format. |
| 4 — runtime | `moor-kernel` | The task scheduler, the builtin functions, and the VM host that binds the VM to the world state. Most of the system's behaviour is here. |
| 5 — services | `moor-daemon` | The daemon process: the RPC server, connection registry, event log, worker supervision, and system control. |
| 5 — hosts | `moor-telnet-host`, `moor-web-host`, `moor-mcp-host` | One user-facing protocol each: line-oriented TCP, HTTP and WebSocket, and the Model Context Protocol. |
| 5 — workers | `moor-curl-worker`, `moor-file-worker` | Out-of-process capabilities. Outbound HTTP, and sandboxed filesystem access. |
| 6 — assembly | `moor-server` | The single-process build. It links the daemon and the hosts into one binary. |

## Binaries

| Binary | Crate | Role |
|---|---|---|
| `moor-daemon` | `moor-daemon` | The shared object environment and execution engine. One per world. |
| `moor` | `moor-server` | Daemon and hosts in one process. Use it for development and for small deployments. |
| `moor-mcp-host` | `moor-mcp-host` | Exposes a world to an MCP client. |
| `moorc` | `tools/moorc` | Offline compilation and database checking. |
| `moor-emh` | `tools/moor-emh` | Repair and inspection of a database that will not start. |

The telnet and web hosts also build to binaries. Confirm the current binary list with
`cargo build --workspace` output, or by reading the `[[bin]]` sections.

## Test and tool crates

| Path | Purpose |
|---|---|
| `crates/testing/moot` | The Moot harness. Runs declarative MOO test files against a live world. |
| `crates/testing/lambdamoo-harness` | Compatibility testing against LambdaMOO behaviour. |
| `crates/testing/load-tools` | Load and throughput exercises. |
| `tools/moorc`, `tools/moor-emh` | Workspace members, listed above. |
| `tools/example-python-worker` | A worker written outside Rust. Read it to learn the worker protocol from the outside. |
| `tools/moo-mode.el`, `tools/moot-lang`, `tools/moot-translate.awk` | Editor and test-authoring support. |
| `tools/generate-api-docs.py`, `tools/perf` | Documentation generation and performance measurement. |

## Not Rust

| Path | Content |
|---|---|
| `book/` | The mdBook manual. It is the best introduction and it is in places behind the code. |
| `doc/` | Design notes and protocol specifications. |
| `clients/` | Client applications and SDKs, including the web client and a Flutter client. |
| `cores/` | Bundled database cores, including `cowbell`, `lambda-moor`, and `minimal-core`. |
| `deploy/`, `docker-compose.yml`, `process-compose*.yaml`, `scripts/` | Ways to bring the system up. |

## How to check this map

This map goes stale when crates are added, split, or renamed. To rebuild it, read the
`members` list in the root `Cargo.toml` and the `description` field of each member's
own `Cargo.toml`. The layer of a crate is decided by which workspace crates it depends
on, which is also in its manifest.
