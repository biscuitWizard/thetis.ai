# Where the mooR documentation disagrees with the code

Observed on 2026-08-29, by reading both. Each row was checked against the source at
that date.

Use this list in one way only: as a warning that a specific document is not
authority on a specific point. Do not cite a row as a current defect without
checking it again. The project is active, and a row here is a good candidate to
have been fixed since.

When you find a new disagreement, the rule is the same one the whole library
follows: **the code is correct, the document is a claim**. Record the new row here,
and in the skill that owns the subject.

## The book

| Document | The claim | What the code does |
|---|---|---|
| `the-system/performance-and-concurrency.md` | Serializable isolation | Snapshot isolation with write-write conflict detection. Read sets are not tracked, so write skew is possible. `the-database/transactions.md` states this correctly in the same book. |
| `the-database/transactions.md`, `the-system/moo-tasks.md` | A conflicting task re-executes from the beginning | A retry restores the task from the state saved at its last transaction boundary. Only a task that never crossed one runs again in full. |
| `the-system/server-assumptions-about-the-database.md` | A restart always restores the previous state | The world-state keyspace is never fsynced. An unclean stop can lose commits the batch writer had not reached. |
| `the-system/server-assumptions-about-the-database.md` | Checkpoints can be written in two formats | Checkpoints are always objdef. The export-format option is deprecated, hidden, and ignored. |
| `the-system/server-assumptions-about-the-database.md` | The server-options table | It omits the garbage-collection interval, and does not say that the dump and collection intervals are read from the system object itself rather than from the server-options object. |
| `the-system/controlling-the-execution-of-tasks.md` | Tick and second limits below a floor are ignored | No floor exists. The scheduler config accepts any non-negative value. |
| `the-system/controlling-the-execution-of-tasks.md` | A queued-task limit raises a quota error on fork or suspend | Not implemented anywhere in the tree. |
| `the-moo-programming-language/task-permissions-and-capability-grants.md` | The list of grant kinds | It omits the object-listing grant, which the parser accepts and which gates object enumeration. |
| `the-built-in-command-parser.md` | Punctuation aliases are applied before the core's command hook | They are applied inside the default parser, which runs only after that hook declines. The hook sees the raw punctuation. |
| `the-database/moo-value-types.md` | The list of value kinds | It omits booleans, which exist and are enabled by default. |
| `the-database/flyweights.md` | Flyweights are garbage collected | They are reference counted. There is no cycle collector for them. |
| `the-system/objdef-file-format.md` | Run the migrate target | No core provides that target. |
| `the-system/object-loading.md` | A dry run makes no changes | The single-object loader mutates its transaction and only declines to commit. Safe on the daemon import path, where the transaction is dropped. Inside a task it commits with the task. |
| `the-system/server-architecture.md` | Single-process components talk over in-process ZeroMQ | The local event loop is a no-op and those endpoint strings are vestigial. |
| `moor-architecture.md` | Module paths inside one daemon crate | The workspace was split into many crates. The chapter is right about the shape and wrong about the paths. |
| `single-process-deployment.md` | The single-process binary lives in the daemon crate | It is in the server crate. |

## Design notes

| Document | The claim | What the code does |
|---|---|---|
| `doc/messaging.md`, `doc/RPC_API_SPEC.md` | Hosts and workers carry their own tokens | Those token types do not exist in the tree. Host and worker identity is the transport's CURVE key plus the allowed-hosts registry. |
| `doc/messaging.md`, `doc/RPC_API_SPEC.md` | Schema sources live under the common crate, and include a database schema file | The schema sources are in the schema crate. That file does not exist. |

## Repository rules and READMEs

| Document | The claim | What the repository does |
|---|---|---|
| `AGENTS.md` | Every file carries a GPLv3 header | Headers are AGPL-3.0 by default, with exceptions. |
| `AGENTS.md` | The client is under a top-level web-client directory, and the RPC crates are under one rpc directory | The client is under `clients/`. The RPC concern is split across the schema, runtime-api, and zmq-client crates. |
| `AGENTS.md` | Run the formatter script at the repository root | It is in `scripts/`. |
| `AGENTS.md` | Test the harness by its short package name | The package name carries the project prefix, and the suites live in other crates. |
| `AGENTS.md` | Root npm scripts for test, lint and typecheck | They do not exist at the root. |
| `CONTRIBUTING.md` | A specific Rust version | It disagrees with the workspace `rust-version` and with CI. Trust the workspace manifest. |
| `flake.nix`, `Dockerfile.arm64-cross`, `Dockerfile.elle` | Their pinned Rust version | Behind the workspace `rust-version`. |
| `cores/lambda-moor` README and makefile | A textdump output option on the offline compiler | That option does not exist, so the default target fails. |
| `crates/kernel/testsuite/README.md` | Anonymous objects are unsupported | They exist behind a feature flag, with a collector. |

## Comments inside the source

| Where | The claim | What happens |
|---|---|---|
| The stored-program module | Decoding happens in the compiler crate | It happens in the schema crate. |
| The opcode-stream module | The program counter indexes the encoded word stream | At run time it indexes the decoded operation list. Jump labels are operation indices. |
| The database config | Settings for object-contents and object-children tables | Those relations no longer exist. Both are reverse lookups on secondary indexes. |
| The RPC client arguments | The two worker socket help strings | They describe the sockets the wrong way round, relative to the daemon's own flags and to actual use. |

## Stray artefacts

A generated schema file is tracked at a nested, duplicated path inside the schema
crate. Nothing includes it. It is the result of running the generator from the wrong
directory. Do not edit it and do not copy its path.
