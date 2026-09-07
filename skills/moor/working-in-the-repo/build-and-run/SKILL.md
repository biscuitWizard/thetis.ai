---
name = "Building and running mooR"
brief = "Compile the mooR workspace and get a server up: which crate makes which binary, cargo profiles and features, and the ways to start daemon, hosts and workers."
when_to_use = "Use when you must compile mooR, pick the right build for the area you changed, or bring a server up locally with telnet and web access. Use it when a build is slow, the stack will not start, or the database will not import. Not for choosing or writing tests, or style and pull-request norms, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "build", "cargo", "run", "daemon", "telnet-host", "web-host", "docker", "process-compose", "bacon", "binaries", "profiles", "startup"]
related = ["moor/services/daemon-and-rpc", "moor/content-pipeline/cores-and-bootstrap"]
version = 2
---

# Building and running mooR

The workspace compiles a small set of binaries. Most of the code is in
libraries; the binaries are thin. Knowing which library your change is in tells
you which binary must be rebuilt and which loop is fastest.

## The build graph

Libraries, in dependency order. Each depends on those above it.

| Crate | Package | Responsibility |
|---|---|---|
| `crates/var` | `moor-var` | MOO value types, symbols, errors |
| `crates/common` | `moor-common` | Object model, world-state interfaces, command matching, shared utilities |
| `crates/compiler` | `moor-compiler` | MOO grammar, parser, code generation, decompiler and unparser |
| `crates/vm` | `moor-vm` | Execution core: frames, activations, unwinding. No scheduler and no host |
| `crates/db` | `moor-db` | The transactional object store |
| `crates/kernel` | `moor-kernel` | Task scheduler, builtin functions, the wiring of VM to database |
| `crates/objdef`, `crates/textdump` | `moor-objdef`, `moor-textdump` | Import and export formats |
| `crates/schema` | `moor-schema` | FlatBuffers types and conversions for wire and persistence |
| `crates/runtime-api` | `moor-runtime-api` | Typed runtime and worker API, shared message types |
| `crates/zmq-client` | `moor-zmq-client` | The ZeroMQ transport that implements that API |
| `crates/daemon` | `moor-daemon` | The runtime assembly: database, scheduler, connections, event log, RPC server |

Binary-producing crates.

| Crate | Binary | What it is |
|---|---|---|
| `crates/server` | `moor` | The single process. Runtime plus telnet host plus web host plus selected embedded workers |
| `crates/daemon` | `moor-daemon` | The split-process runtime. No player-facing protocol |
| `crates/telnet-host` | `moor-telnet-host` | Line-oriented TCP and telnet, optionally TLS |
| `crates/web-host` | `moor-web-host` | HTTP, WebSocket and WebRTC, plus the web APIs |
| `crates/curl-worker` | `moor-curl-worker` | Outbound HTTP on behalf of the world |
| `crates/file-worker` | `moor-file-worker` | File access on behalf of the world |
| `crates/mcp-host` | `moor-mcp-host` | Model Context Protocol host |
| `tools/moorc` | `moorc` | Offline compiler and test runner |
| `tools/moor-emh` | `moor-emh` | Offline database console |

Two more workspace members exist for tests only: `crates/testing/moot`
(`moor-moot`, the text-test harness) and `crates/testing/load-tools`
(`moor-model-checker`, which produces many load and consistency binaries).

The build graph explains the cost of a change. An edit in `moor-var` rebuilds
everything. An edit in `moor-web-host` rebuilds one binary.

## The fastest correct loop, by area

| You changed | Fastest loop | Before you claim it works |
|---|---|---|
| `var`, `common`, `compiler`, `vm` | `cargo test -p <package>` | `cargo test --workspace`, because these are under everything |
| `db` | `cargo test -p moor-db` | Add `cargo test -p moor-kernel`; the kernel suites exercise the store hardest |
| `kernel` | `cargo test -p moor-kernel --test moot-suite` | The full `-p moor-kernel` run, then the workspace |
| `daemon`, `runtime-api`, `zmq-client` | `cargo test -p moor-daemon` | `cargo test -p moor-telnet-host`, which starts a real daemon |
| `telnet-host`, `web-host` | `cargo test -p <package>`, then run the stack | Start `moor` and connect for real |
| `schema` (`.fbs` files) | Regenerate bindings, then `cargo build --workspace` | Also `npm run schema:build`; see `moor/services/wire-schema` |
| `clients/meadow` | `npm run meadow:dev` | `npm run web:build` and `npm run web:typecheck` |
| `cores/cowbell` | `make -C cores/cowbell` | `make -C cores/cowbell test` |

## Build commands

`cargo build` with no arguments builds the workspace `default-members`. That set
deliberately excludes the proc-macro helper crates and `lambdamoo-harness`,
which needs external C sources fetched first. Use it as the everyday build.

`cargo build --workspace` builds everything, including `lambdamoo-harness`. CI
passes `--exclude lambdamoo-harness` for exactly that reason. If a plain
`--workspace` build fails on a C compile, that is the harness, and you almost
certainly did not want to build it.

`cargo build -p <package>` is the tight loop. Prefer it.

### Profiles

| Profile | Purpose |
|---|---|
| `dev` (default) | Development. Fast to compile, slow to run |
| `release` | Single codegen unit, fat LTO, full debug info. Slow to build, fastest to run, and profilable |
| `release-fast` | Inherits `release` with thin LTO, default parallelism and no debug info. For packaging and container builds where build time matters |

Never benchmark or measure a performance claim on a `dev` build. The `release`
profile keeps debug information on purpose so that a profiler can attribute
time; that is why release binaries are large. [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling)
owns how to measure.

### Features

Feature flags are few. `trace_events` is the one that matters: it exists in
`moor-vm`, `moor-kernel`, `moor-daemon` and `moor-server`, and emits Chrome
Trace Event Format records for offline analysis. Build with it only when you are
tracing; [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling)
says when that is worth doing. Consult a
crate's `Cargo.toml` for its current feature list rather than trusting a written
list here.

### Build inputs that are not source

The `moor-common` build script reads the git checkout to stamp a version and a
commit hash into every binary. A build from a source tree with no `.git`
directory can fail or produce an unstamped binary. The Dockerfile copies `.git`
into the build stage for this reason. That stamp is what a bug report is asked
to quote from the daemon's first log line.

FlatBuffers Rust bindings are generated ahead of time and committed. A `.fbs`
change is not picked up by `cargo build`; you must regenerate. The TypeScript
bindings are generated at npm build time and need the `flatc` compiler on the
path.

## Bringing a system up

Reach for the first row first.

| Way | Command | What it is for |
|---|---|---|
| Single process, from cargo | `npm run moor:dev` | The default development loop. One `moor` process, telnet and web, importing the Cowbell core |
| Single process plus web client | `npm run full:dev` | Adds the Meadow dev server with hot reload. Use when you touch the browser client |
| File-watching restart | `bacon` (default job), or `bacon daemon`, `bacon telnet`, `bacon web` | Rebuild and restart on every save. Use when iterating on one host |
| Split processes, from cargo | `process-compose up` | Exercises the real ZeroMQ path between daemon, hosts and worker. `process-compose-dev.yaml` is the same with debug builds |
| Containers | `docker compose up` | A container image close to what is released. Slow to build, honest about packaging |
| Deployment examples | The trees under `deploy/` | Clustered, Kubernetes, Debian package and TLS shapes. Not a development loop; see [deployment-and-release](skill:moor/working-in-the-repo/deployment-and-release) |

`npm run moor:dev` and the default `bacon` job run the same thing: the `moor`
binary against `moor-dev.yaml`, importing `cores/cowbell/src` in `objdef`
format, with a generated key pair and the curl worker enabled. `moor-dev.yaml`
is the checked-in development configuration and turns on the newer language
features. Override the config with the `MOOR_CONFIG` environment variable and
the core with `MOOR_CORE`.

`process-compose` and the bacon `daemon` job import `cores/lambda-moor/src`
instead, which is the reconstituted LambdaCore. The two cores behave very
differently; know which one you started.

### What a running system is, as processes

In the single-process shape there is exactly one operating-system process. Its
internal boundaries are the same ones the split shape has: hosts talk to a
runtime through a typed in-process client rather than through ZeroMQ. Sessions,
authentication and connection semantics are identical. This is deliberate, so
that a bug found in one shape is a bug in both.

In the split shape there are four or more processes:

| Process | Role |
|---|---|
| `moor-daemon` | Owns the database, the scheduler, the VM, connections, the event log, host enrolment and worker routing. Serves RPC |
| `moor-telnet-host` | Accepts telnet clients. Talks request and reply plus subscribe to the daemon |
| `moor-web-host` | Accepts HTTP, WebSocket and WebRTC clients. Same two channels |
| `moor-curl-worker`, `moor-file-worker` | Perform outbound effects the world asks for. Subscribe for requests, reply over their own channel |
| Meadow (a static bundle, served by nginx or Vite) | The browser client. Not a server process |

Hosts and workers may come and go while the daemon stays up. That is the point
of the split. [daemon-and-rpc](skill:moor/services/daemon-and-rpc) explains the
enrolment and authentication that makes it safe.

### Persistent state on disk

The data directory, `moor-data` by default, holds the database. The `--db` value
names the database inside it. The import runs only when the database does not
yet exist. To force a fresh import, delete the data directory; `scripts/daemon.sh
--clean-slate` does that for you.

The key pair signs session tokens. `--generate-keypair` creates it on first run
if it is absent. Generated keys and exports must not appear in a diff.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A `--workspace` build fails compiling C | `lambdamoo-harness` is in the build | Drop `--workspace`, or add `--exclude lambdamoo-harness` as CI does |
| The build succeeds locally and fails in CI | Local toolchain is newer, or a feature combination is untested locally | CI builds with `--all-features --all-targets` on the pinned toolchain. Reproduce with those flags before you look further |
| Everything rebuilds after a one-line edit | The edit was in `moor-var` or `moor-common` | Expected. Work in the leaf crate and its tests, and pay the full rebuild once |
| A `.fbs` edit changes nothing | Rust bindings are committed, not generated by `build.rs` | Regenerate the bindings and commit them. See [wire-schema](skill:moor/services/wire-schema) |
| The npm build fails looking for `flatc` | The FlatBuffers compiler is missing | Install it, or set `MOOR_FLATC` to its absolute path |
| The server starts but the world is empty or wrong | The data directory already existed, so the import was skipped | Delete the data directory and start again |
| The server starts with unexpected verbs or objects | You imported a different core than you expected | Check `MOOR_CORE`, `moor-dev.yaml` and which launcher you used |
| A host cannot reach the daemon | Socket addresses do not match | The launchers agree on IPC socket paths. If you started processes by hand, the RPC and events addresses must be given to both sides |
| A container build takes very long | The default container profile is an optimising build | Pass `BUILD_PROFILE=release-fast`, or use a cargo-based launcher instead |
| A performance number looks wrong | You measured a `dev` build | Rebuild with `--release` and measure again. See [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling) |

## Read first / read next

Read [repo-tooling](skill:moor/working-in-the-repo/repo-tooling) for what
`bacon`, `process-compose`, `moorc` and the `scripts/` wrappers actually are.
Read [testing](skill:moor/working-in-the-repo/testing) before you assert that a
change works, and [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling)
if the claim is about speed. Read
[deployment-and-release](skill:moor/working-in-the-repo/deployment-and-release)
for the container and package shapes, and
[cores-and-bootstrap](skill:moor/content-pipeline/cores-and-bootstrap) before
you change import or startup behaviour.

## Verify this still holds

```
cargo build -p moor-server
npm run moor:dev
```

The server logs its version and commit stamp on the first line; if that line
is missing or unstamped, the `.git` directory was not visible to the build.
