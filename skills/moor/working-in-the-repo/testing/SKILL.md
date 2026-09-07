---
name = "Testing mooR"
brief = "Choose and run the right mooR test: cargo unit tests, .moot text tests, cross-process integration tests, benches, Elle consistency runs, and what CI gates."
when_to_use = "Use when you must add a test, decide whether a behaviour belongs in a Rust test or a .moot file, run a single test, test something that spans the daemon and a host, or explain a test that passes alone and fails in the workspace. Use it before you claim a change works. Do not use it for compiling or starting a server; that is build-and-run. Do not use it for style, licence headers or pull-request rules; that is conventions. Not for the Torchship game database, for authoring MOO verbs inside a running world, or for Thetis's own internals."
universal = false
tags = ["moor", "test", "moot", "cargo test", "integration test", "proptest", "bench", "elle", "jepsen", "regression", "ci", "flaky"]
related = ["moor/execution/task-scheduler", "moor/storage-and-state/transactions"]
version = 1
---

# Testing mooR

mooR has four test kinds and they are not interchangeable. Choosing wrongly
produces a test that is slow, fragile, or that does not exercise the thing you
changed. This skill tells you which to pick.

The project's stated testing values: exercise real logic rather than mocks, keep
test output clean, and prefer a single small failing case over a broad one.

## The four kinds

| Kind | Lives in | Exercises | Cost |
|---|---|---|---|
| Unit test | A `#[cfg(test)]` module beside the implementation | One Rust function or type | Milliseconds |
| Moot test | A `.moot` text file under a crate's test tree | MOO language and runtime behaviour, as a player or programmer sees it | Fast in-process; slow over a socket |
| Integration test | A crate's `tests/` directory | A crate boundary, or several processes together | Seconds to tens of seconds |
| Load and consistency tool | `crates/testing/load-tools` | Concurrency, isolation and throughput under many tasks | Minutes |

Anything that is only visible when the whole system runs, such as the browser
client or a deployment shape, is checked by running it and saying so in the pull
request.

### When each is right

Write a **unit test** when the answer is a pure function of its inputs: a parse,
an encoding, a comparison, a bit of matching logic. Most crates are full of
these. They are the first thing to add and the cheapest to keep.

Write a **moot test** when the behaviour is what MOO code observes. Anything
expressible as "evaluate this and the result should be that", or "run this
command and the player should see that", belongs in a `.moot` file. This is
where language semantics, builtin behaviour, error values, permission
enforcement and LambdaMOO compatibility are asserted. A moot file is far easier
to read and to extend than the equivalent Rust, and it is the format the ported
LambdaMOO and Stunt regression suites already use.

Write an **integration test** when the thing under test is a boundary: a crate's
public surface, an import and export round trip, or the behaviour of two
processes talking to each other. Reach for one only when a moot test cannot see
what you need.

Reach for a **load tool** when the property is about concurrency: does the store
stay serializable, does the scheduler stay fair, does throughput regress. Those
tools are also the honest way to measure a performance change in the database or
the scheduler; `performance-and-profiling` covers that use.

## Moot

Moot is a text format for MOO interaction tests, parsed by the `moor-moot`
crate. A moot file is a script of inputs and expected outputs. Each line is one
of a small set of forms: switch the acting player to programmer or wizard,
evaluate an expression, run a command, assert the next response, assert an exact
output line, or comment. The current form list is the table in
`crates/testing/moot/README.md`; read it before writing a file.

Two properties of the format matter more than the syntax.

**Assertions are positional, not associated.** An assertion consumes the next
output the server produces, whatever produced it. This is what lets a moot file
test flows that involve `read()` and suspended tasks. It also means that output
you did not expect shifts every later assertion, and that output nothing
consumed fails the test at the end of the file.

**Most assertions are evaluated as MOO expressions.** The expected value is put
through another round of evaluation, so an assertion may name variables such as
`player`, and a string literal must carry its quotes. The exact-line form is the
exception: it compares raw text and does no evaluation.

### The two runners

| Runner | Where | Transport | Exact-line assertions |
|---|---|---|---|
| Scheduler runner | `moor-kernel`, test target `moot-suite` | Calls the scheduler in process | Not supported |
| Telnet runner | `moor-telnet-host` integration test, and `moor-moot`'s own LambdaMOO comparison test | A real TCP socket to a real host process | Supported, and preferred there |

The scheduler runner is the one you will use for almost every new moot file. It
starts a scheduler over a fresh database, with no session and no network, so it
is fast enough to run a whole suite on every build. Put a moot file under the
kernel test tree and it is discovered automatically; there is no list to edit.

The telnet runner exists because some behaviour is only true over a connection:
line handling, narrative delivery, `read()` against a real client, output
ordering. Use it only for those. Each such test starts a daemon and a telnet
host as child processes, so it is slow and it holds a fixed port.

`moorc` is a third way to run moot files: it can compile an objdef source tree
and then run `.moot` files against it. That is how a core's own tests run, and
it is the right tool when the behaviour under test depends on the core rather
than on the server.

## Where tests live

| Path | Contains |
|---|---|
| `crates/*/src/**` in `#[cfg(test)]` modules | Unit tests, throughout |
| `crates/compiler/src/tests/proptest/` | Property tests for the compiler, including a parse and unparse round trip |
| `crates/kernel/testsuite/moot/` | The main moot suite, discovered by directory walk |
| `crates/kernel/testsuite/regression_suite.rs` | Rust-level regression cases that moot cannot express |
| `crates/kernel/src/testing/` | Helpers the kernel's own tests share: VM and scheduler drivers |
| `crates/daemon/src/testing/` | Daemon test environment, RPC and scheduler integration tests, event-log backend parity |
| `crates/db/tests/` | Save and restore, and replay of recorded concurrent histories |
| `crates/telnet-host/tests/` | Daemon plus host, over a real socket, driving `.moot` files |
| `crates/server/tests/` | A smoke test that starts the single-process binary, connects by telnet and by HTTP, and shuts it down |
| `crates/textdump/tests/`, `crates/compiler/tests/` | Format round trips and deep-structure cases |
| `crates/*/benches/` | Benches for value types, the store, the compiler and dispatch paths. See `performance-and-profiling` |
| `crates/testing/load-tools/` | Load generators and Elle history producers |
| `crates/testing/lambdamoo-harness/` | Optional comparison against the original C LambdaMOO |
| `cores/cowbell/tests/` | MOO-level tests for the Cowbell core, run by `moorc` through its Makefile |

The kernel's moot suite carries a `README.md` that tracks which Stunt test files
have been ported and which are deliberately not. Read it before adding a moot
file that duplicates one.

## Running tests

Whole workspace: `cargo test --workspace`. Add `--exclude lambdamoo-harness`, as
CI does, unless you have fetched the C sources.

One crate: `cargo test -p moor-kernel`. One target: `cargo test -p moor-kernel
--test moot-suite`. One test by name: append the name after the target.

The moot suite has an ignored single-file test, kept so that one moot file can
be run alone under a debugger or a profiler. Its own comment gives the exact
invocation; read the tail of the kernel moot suite source when you need it.

Debug output: `cargo test -- --nocapture`. The moot runners print the exchange
they drive, which is usually enough to see where a file went wrong.

Consistency runs are two steps: a workload binary from `moor-model-checker`
writes an EDN history, and the external `elle-cli` checker verifies it is
serializable. CI does this for the list-append and read-write-register models.
The scripts in `crates/testing/load-tools` and `Dockerfile.elle` run the same
thing locally.

## Testing something that spans the daemon and a host

Three levels, cheapest first.

1. **Can the behaviour be expressed as MOO input and output?** Then it is a moot
   file under the kernel suite, and no host is involved.
2. **Does it depend on the connection?** Then it is a moot file under the telnet
   host's test tree, driven by the telnet runner. The test starts a daemon and a
   host for you.
3. **Does it depend on the process boundary itself** — enrolment, reconnection,
   worker routing, event delivery? Then it is a Rust integration test in
   `crates/daemon/tests` or the daemon's testing module, or the single-process
   smoke test in `crates/server/tests` if it is about the combined binary.

Note the shape of the second level, because it has a trap. The telnet host test
looks for an already-built `moor-daemon` binary next to the test executable and
uses it if present, and only builds one when it is absent. If you change daemon
code and re-run only the telnet host test, you may be testing the old daemon.
Build `moor-daemon` yourself, or remove the stale binary, before you trust the
result.

## What CI gates

CI runs the following. Match it locally before you submit.

| Job | What it runs |
|---|---|
| Build and tests | A `--all-features --all-targets` build, then the workspace test run, then documentation tests. Excludes `lambdamoo-harness` |
| Formatting and clippy | Nightly rustfmt in check mode with the project's import settings, then clippy across all targets and features with warnings denied |
| Web | Builds and type-checks the npm workspace, and checks repository formatting |
| Cowbell core | Builds and tests the Cowbell core through its Makefile |
| Licence headers | Runs `licensure` in check mode over tracked files |
| Elle consistency | Generates two workloads and verifies both histories are serializable |
| Deployment validation | Shell syntax, rendering of every compose file and the Kubernetes manifests, and a frontend image build |

Documentation tests are a separate CI step. A doc comment with a fenced Rust
example is compiled and run. Take care when you add one.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A moot file fails at the last line with unconsumed output | Something produced more output than the file asserts | Add the missing assertion, or find what is narrating extra text |
| A moot assertion fails with a value that looks correct | The expected side is evaluated as MOO, so a string needs quotes and a type must match | Write the expected value as a MOO literal |
| An exact-line assertion is reported unimplemented | The file runs under the scheduler runner, which has no network | Move the file to the telnet host suite, or change to a value assertion |
| A test passes alone and fails in the workspace run | Shared state: a fixed port, a fixed socket path, a shared temporary file | Cargo runs tests in threads. Use a unique port or path, or mark the test serial as the existing socket-bound tests do |
| A telnet host test shows behaviour you already fixed | It reused a stale `moor-daemon` binary from the target directory | Rebuild `moor-daemon`, or delete it so the test builds a fresh one |
| A test in `lambdamoo-harness` will not compile | The external LambdaMOO sources were never fetched | Run its setup script, or exclude the crate |
| Clippy fails only in CI | CI passes `--all-targets --all-features` and denies warnings | Run clippy with the same flags before submitting |
| An Elle run reports a cycle | Either a real isolation bug, or a workload bug | Do not dismiss it. Keep the EDN history; it is the evidence. Read `moor/storage-and-state/transactions` |
| A bench result moves a lot between runs | Machine noise, or a debug build | Read `performance-and-profiling`, which owns measurement method and its failure branches |

## Read first / read next

Read `crates/testing/moot/README.md` in the repository before writing your first
moot file, and the kernel testsuite `README.md` before porting a Stunt test.
Read `conventions` for the gates a change must pass. Read
`moor/storage-and-state/transactions` before interpreting a consistency failure,
and `moor/execution/task-scheduler` before writing a test that suspends or forks
a task. Read `performance-and-profiling` when the question is speed rather than
correctness.
