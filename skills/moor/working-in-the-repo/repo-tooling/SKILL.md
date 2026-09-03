---
name = "mooR repository tooling"
brief = "What each tool in tools/ and scripts/ is for and who needs it: moorc, moor-emh, bacon, process-compose, the perf scripts, the book, and the schema generators."
when_to_use = "Use when you need a tool and do not know which: compiling a MOO source tree offline, inspecting a database without a server, watching files and restarting, or generating FlatBuffers or documentation. Use it also to know which tools must be installed before a first change. Not for choosing a build or a test, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "tools", "moorc", "moor-emh", "bacon", "dprint", "licensure", "mdbook", "process-compose", "perf", "profiling", "flatc", "planus", "scripts", "editor", "watching files", "api-reference documentation", "moot syntax support"]
related = ["moor/services/wire-schema", "moor/content-pipeline/objdef-format"]
version = 2
---

# mooR repository tooling

The repository ships two kinds of tool: programs built from the workspace, and
external programs the project expects you to install. This skill says what each
is for and who needs it, so that you install four things rather than twenty.

## Install before your first change

| Tool | Why | How |
|---|---|---|
| The pinned stable Rust toolchain | Everything builds on it | `rustup`. The version is `rust-version` in the root `Cargo.toml` |
| The nightly toolchain, rustfmt component only | Formatting uses nightly-only import options | `rustup toolchain install nightly` |
| `dprint` | Formats JSON, TOML, Markdown, TypeScript and Dockerfiles | It is an npm dev dependency, so `npx dprint` works after `npm ci` |
| `licensure` | Checks and writes licence headers | `cargo install licensure`. CI pins a version; match it if a header check disagrees |
| Node.js and npm | The browser client, the TypeScript schema bindings, the formatting check | Any recent LTS |

Install when the task needs them, not before: `bacon`, `process-compose`,
`flatc`, `planus`, `mdbook` and its plugins, Docker, `perf`.

A Nix flake is present and `direnv` is wired to it. It is convenient but it is
not authoritative: it pins an older Rust than the workspace requires. If you use
it, check the toolchain version before you trust a build.

## Tools built from the workspace

### `moorc` — the offline compiler and test runner

`moorc` compiles MOO source without a server. It reads either an objdef
directory tree or a textdump file, and writes an objdef directory. It is the
tool that validates a core: if `moorc` compiles a core cleanly, the core's
source is well formed.

It is also a test runner, and this is why it matters beyond compilation. It can
run every verb whose name begins with `test_` in the compiled database, filtered
by object or by verb, with a per-test timeout. It can also run `.moot` files
against the compiled database, from a directory or a glob. This is how the
Cowbell core's own test suite runs, driven by its Makefile, and it is what CI
runs for that core.

Who needs it: anyone changing a core, anyone changing the compiler, anyone
importing a LambdaMOO or ToastStunt database. It has a flag to continue past
verbs that fail to compile, which is how a foreign database is brought in for
inspection. Run it with `--help` for the current flag set rather than trusting a
list here.

### `moor-emh` — the offline database console

An interactive console over a database directory, with no daemon and no network.
It opens the store, starts a scheduler, and gives you a line editor in which to
evaluate MOO. It takes a lock on the database, so it is for a database nothing
else is serving.

Who needs it: anyone who must inspect or repair a database that will not boot,
or who wants to evaluate against a database without bringing a server up.

### Load and consistency binaries

`crates/testing/load-tools` builds under the package name `moor-model-checker`
and produces many binaries: Jepsen-style workload generators that write EDN
histories for the external Elle checker, and several load and benchmark drivers
for verb dispatch, property updates, opcodes, the scheduler and suspend-resume.
The binary list is the `[[bin]]` sections of its `Cargo.toml`; read those rather
than a list here. Its own `README.md` and the shell scripts beside it explain
the Elle workflow. See [testing](skill:moor/working-in-the-repo/testing).

### `lambdamoo-harness`

An optional comparison harness that builds the original LambdaMOO C server and
runs the same tests against both. It is excluded from the default build and from
CI because it needs external sources fetched first, by its own setup script.
Reach for it only when the question is "what does the real LambdaMOO do here".

## Non-Rust tools in `tools/`

| Path | What it is | Who needs it |
|---|---|---|
| `tools/generate-api-docs.py` | Reads the web host's OpenAPI specification and writes the HTTP API reference page of the book | Anyone changing a web host route. Needs PyYAML |
| `tools/moo-mode.el` | Emacs major mode for MOO files: syntax highlighting and indentation | Emacs users editing `.moo` files |
| `tools/moot-lang/` | A Visual Studio Code extension that highlights `.moot` files | Anyone writing many moot tests in that editor |
| `tools/moot-translate.awk` | A one-way helper that rewrites Ruby test files from the Stunt suite towards moot syntax | Anyone porting another Stunt regression file. It gets most lines right and leaves the rest to you |
| `tools/perf/` | Shell wrappers around Linux `perf`, and the activation profiling binary they drive | Anyone investigating a performance regression on Linux. [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling) explains what they produce |
| `tools/example-python-worker/` | A worker implemented in Python, over the same FlatBuffers and ZeroMQ protocol the Rust workers use | Anyone writing a worker in another language, or checking that the worker protocol is genuinely language-neutral |

The API reference page is generated. Do not hand-edit it; change the OpenAPI
specification and re-run the generator.

## `scripts/`

Thin wrappers that supply the correct arguments so you do not have to remember
them.

| Script | Does |
|---|---|
| `scripts/format-rust.sh` | Runs nightly rustfmt with the project's three import options. Takes `--check`. **Use this, not plain `cargo fmt`** |
| `scripts/licensure-project.sh` | Runs licensure over git-tracked files only, so untracked and generated files are left alone |
| `scripts/daemon.sh` | Runs `moor-daemon` with a data directory, a core, an export path and a checkpoint interval. Has `--debug`, `--traced` and `--clean-slate`, and reads several environment variables |
| `scripts/telnet.sh`, `scripts/web.sh`, `scripts/curl-worker.sh` | The matching host and worker launchers |
| `scripts/start-moor-lambdacore.sh`, `scripts/start-moor-cowbell.sh` | Start a server against one specific core |

## External development tools

### `bacon` — watch and restart

`bacon.toml` defines named jobs. The default job runs the single `moor` binary
against the development configuration. Other jobs run the daemon in release or
debug, the daemon with tracing, the telnet host with and without TLS, the web
host, the curl worker, and the test suite. Read `bacon.toml` for the current job
list; it changes more often than this skill.

Use bacon when you are iterating on one component and want it rebuilt and
restarted on every save.

### `process-compose` — the split stack without containers

`process-compose.yaml` brings up the daemon, the telnet host, the web host, the
curl worker and the frontend dev server as separate host processes, wired over
IPC sockets, with restart-on-failure and start-ordering between them.
`process-compose-dev.yaml` is the same with debug builds and debug logging.

Use it when the thing you changed lives on the boundary between processes. It is
the cheapest way to exercise the real ZeroMQ path.

### Docker

`Dockerfile` at the root builds the frontend in a Node stage and the Rust
binaries in a Rust stage, then assembles a slim runtime image. Build arguments
select the cargo profile, the job count and whether tracing is compiled in.
`docker-compose.yml` runs the single-process image plus the frontend in nginx.
`Dockerfile.elle` builds the consistency-check environment.
`Dockerfile.arm64-cross` cross-builds for arm64.

`deploy/` holds the deployment examples rather than development stacks. CI
renders every one of those manifests on each push, so a syntax error there fails
the build even though nothing was deployed. [deployment-and-release](skill:moor/working-in-the-repo/deployment-and-release)
owns that whole area.

### The book

`book/` is an mdBook. `book/install-tools.sh` installs mdBook and the two
plugins the book uses, at the versions the book expects; run it before building
the book. `book/build-single-page.sh` produces a single PDF through pandoc.

Edit `book/src/` and add the page to `book/src/SUMMARY.md`, or it will not
appear. A change to user-visible behaviour must update the book in the same pull
request; see [conventions](skill:moor/working-in-the-repo/conventions).

### The schema generators

FlatBuffers bindings are generated by two different tools, and the two have
different lifecycles.

| Target | Generator | Committed? |
|---|---|---|
| Rust | `planus` | Yes. Regenerate and commit the generated file with the schema change |
| TypeScript | `flatc`, driven by an npm script | No. Built on demand; set `MOOR_FLATC` if `flatc` is not on the path |

Because the Rust side is committed, a `.fbs` edit alone changes nothing that
cargo can see. [wire-schema](skill:moor/services/wire-schema) covers the
schema itself and the compatibility rules a change must respect.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Formatting still fails after `cargo fmt` | Stable rustfmt ignores the project's import options | Run `scripts/format-rust.sh` |
| `scripts/format-rust.sh` reports no nightly toolchain | Nightly is not installed | Install it with the rustfmt component. It is used for nothing else |
| Licensure rewrites files you did not touch | It ran over the whole tree | Use `scripts/licensure-project.sh` |
| The header check disagrees with your local licensure | Version drift | Match the version CI installs |
| `npm run schema:build` cannot find `flatc` | The FlatBuffers compiler is missing | Install it, or set `MOOR_FLATC` to its absolute path |
| A `.fbs` change has no effect on the Rust build | Rust bindings are committed, not generated at build time | Regenerate with planus and commit the result |
| The book build fails on a missing preprocessor | mdBook plugins are not installed | Run `book/install-tools.sh` |
| The API reference page reverts your edit | That page is generated | Edit the OpenAPI specification and re-run the generator |
| A `perf` script refuses to record | The kernel blocks access to performance counters | The script reports which setting to change. See [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling) |
| A Nix shell build fails on the toolchain | The flake pins an older Rust than the workspace needs | Use rustup, or update the flake |
| `process-compose` starts the wrong world | Its launchers import a different core than the npm and bacon defaults | Check which core each launcher imports before you compare behaviour |

## Read first / read next

Read [build-and-run](skill:moor/working-in-the-repo/build-and-run) for which
launcher to reach for first, and [conventions](skill:moor/working-in-the-repo/conventions)
for the gates these tools enforce. Read [performance-and-profiling](skill:moor/working-in-the-repo/performance-and-profiling)
before using the perf scripts, and [deployment-and-release](skill:moor/working-in-the-repo/deployment-and-release)
before changing anything under `deploy/`. Read
[objdef-format](skill:moor/content-pipeline/objdef-format) before using
`moorc` on a source tree, and [wire-schema](skill:moor/services/wire-schema)
before regenerating bindings.
