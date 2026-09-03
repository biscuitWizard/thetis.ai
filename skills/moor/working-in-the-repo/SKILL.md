---
name = "Working in the mooR repository"
brief = "Build, run, test and contribute to the mooR Rust workspace: which crate makes which binary, the checks CI enforces, and the rules a patch must satisfy."
when_to_use = "Use when the task is about the repository itself: compiling the workspace, starting a server locally, choosing a test to write or run, formatting, clippy, licence headers, dependency policy, commit and pull-request norms, or a build that fails only in CI. Use it before you claim a change works. Do not use it to learn how the database, the compiler, the virtual machine or the RPC protocol behave; those are the other moor topics. Do not use it for the Torchship game database, for authoring MOO verbs inside a running world, or for Thetis's own internals."
universal = false
tags = ["moor", "build", "cargo", "workspace", "test", "clippy", "rustfmt", "dprint", "ci", "contributing", "pull request", "rust"]
children = "auto"
related = ["moor/services/daemon-and-rpc"]
version = 1
---

# Working in the mooR repository

mooR is one Cargo workspace of Rust crates, plus an npm workspace of TypeScript
packages, plus MOO source trees under `cores/`. This skill and its children tell
you how to build it, how to run it, how to test it, and what a patch must satisfy
before it is acceptable.

`AGENTS.md` and `CONTRIBUTING.md` at the repository root are the project's own
statements of these rules. Read them. Where they disagree with the code or the
tooling, the code is right; the "Stale statements" table below lists the
disagreements found so far.

## Which child to read

| Child | Covers | Reach for it when |
|---|---|---|
| `build-and-run` | The build graph, crates to binaries, profiles, features, and every way to start a server | You must compile something, or get a running system in front of you |
| `testing` | Unit tests, `.moot` text tests, integration tests, consistency runs, and what CI gates | You must add a test, choose a test kind, or explain a failure |
| `conventions` | Licence headers, formatting, clippy, dependency policy, code style, commit and pull-request norms | You are about to write or submit a change |
| `repo-tooling` | `tools/`, `scripts/`, bacon, dprint, licensure, the book, the schema generators | You need a tool and do not know which one, or who it is for |
| `performance-and-profiling` | The benches, `perf`, Chrome traces, runtime counters, and what zero-copy means in the hot crates | A change is meant to be faster, or something is slow and you do not know where |
| `deployment-and-release` | `deploy/`, the Dockerfiles, the release workflow, keys and enrollment tokens, and operator facts | You touch a compose file, a manifest, a script or packaging, or you must plan a real deployment |

Sibling skills are named bare in backticks. Skills in other topics are named by
path, such as `moor/execution/task-scheduler`.

## Facts every contributor needs

**The workspace is the unit.** Every crate lives under `crates/` or `tools/` and
is a member of the root `Cargo.toml`. Crate directory names drop the `moor-`
prefix that the package names carry: the directory `crates/kernel` holds the
package `moor-kernel`. Use the package name with `-p`.

**Dependency versions live in one place.** The root `Cargo.toml` declares every
third-party version under `[workspace.dependencies]`. A member crate inherits
with `.workspace = true`. Never write a version number in a member manifest.

**Rust version and toolchain.** The workspace `rust-version` in the root
`Cargo.toml` is the authority; CI installs exactly that toolchain. Stable Rust
builds everything. The nightly toolchain is used for one purpose only:
formatting. Edition is 2024.

**There are two deployment shapes, and both are supported.** A single `moor`
binary runs the runtime and the hosts in one process. A split deployment runs
`moor-daemon` plus separate host and worker processes that speak ZeroMQ and
FlatBuffers. The single process is the default for development. `build-and-run`
explains both.

**Five gates decide whether a change is acceptable.** They are, in the order CI
runs them: the workspace builds, the workspace tests pass, formatting matches,
clippy is clean with warnings denied, and every tracked file carries the right
licence header. `conventions` gives each gate and its command.

**The book is part of the change.** `book/` is the user documentation. A change
to user-visible behaviour updates the book in the same pull request.

## Rules the project states about a coding partner

`AGENTS.md` addresses an AI coding partner directly. These are not suggestions.

1. **Ask before a large change.** You are a coding partner, not an independent
   agent. The human decides. Propose the shape of a large change and get
   agreement before you write it.
2. **Do not run git commands unless asked.** No commits, no branches, no
   rebases, no pushes, unless the human asked for that specific action.
3. **Do not write marketing language.** Not in code, not in comments, not in
   commit messages, not in replies. The word "comprehensive" is named as
   forbidden. Do not claim anything is "production ready".
4. **Do not write legacy-compatibility scaffolding.** The project has no
   installed base to protect. Migration shims and "legacy bridge" paths are an
   anti-pattern here. Change the thing.
5. **Write your own commit messages and pull-request descriptions.**
   `CONTRIBUTING.md` asks that these are not machine-generated.

## Stale statements in the project's own documents

These documents are still the right statement of intent. These specific details
have drifted. Verify a path or a command before you follow it.

| Document says | Repository has |
|---|---|
| The web client is in `web-client/` | The web client is `clients/meadow`; other clients and the browser SDK are also under `clients/` |
| `crates/rpc/` holds the RPC crates | The wire and transport crates are `crates/schema`, `crates/runtime-api` and `crates/zmq-client` |
| `./format-rust.sh` at the repository root | `scripts/format-rust.sh` |
| `cargo test -p moot` | The package is `moor-moot`; the `.moot` suites run under `moor-kernel` and `moor-telnet-host` |
| Source files carry a GPLv3 header | The default header is AGPL-3.0; `.licensure.yml` assigns LGPL to the schema and browser SDK and GPL to the Meadow client |
| The `moor` binary is at `crates/daemon/src/bin/moor.rs` | It is the `moor-server` crate, `crates/server` |
| `npm run test`, `npm run lint`, `npm run typecheck` at the root | The root exposes `web:typecheck` and `format:check`; `lint` and `test` are per-package scripts |
| `flake.nix` pins the project's Rust version | `flake.nix` pins an older Rust than the workspace `rust-version`; trust `Cargo.toml` |

## Knowledge barriers

Before a first change lands, you must understand these. Each has a source.

| You must understand | Learn it from |
|---|---|
| What the workspace builds and what a running system is, as processes | `build-and-run` |
| Which test kind fits the change you made | `testing` |
| The five gates and the style rules | `conventions` |
| How to prove a change made something faster | `performance-and-profiling` |
| Why a deployment file you edited failed CI | `deployment-and-release` |
| The transaction model, if you touch `crates/db` or `crates/kernel` | `moor/storage-and-state/transactions` |
| The task and verb execution model, if you touch scheduling or the VM | `moor/execution/task-scheduler` |
| The FlatBuffers wire contract, if you touch any message between processes | `moor/services/wire-schema` |
| How a database is imported and bootstrapped, if you touch startup or cores | `moor/content-pipeline/cores-and-bootstrap` |
| How to add a builtin function | `crates/kernel/src/vm/builtins/ADDING-BUILTINS.md` in the repository |

## Read first / read next

Read `AGENTS.md` and `CONTRIBUTING.md` in the repository before your first
change. Then read the child that matches your task. If you do not know which
area of the system your change touches, read the root `moor` skill first and let
it route you.
