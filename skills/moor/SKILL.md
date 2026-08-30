---
name = "mooR server"
brief = "Ground truth for the mooR server codebase: the Rust MOO engine, its transactional object database, compiler, VM, task scheduler, and daemon/host processes."
when_to_use = "Use for any task inside the mooR repository at workspace/moor: reading or changing the daemon, database, compiler, VM, scheduler or host processes. Start here to find the right sub-skill. Not for the Torchship game database or any other in-world MOO core, which torchship skills own. Not for writing verbs in a running world. Not for Thetis internals."
universal = false
tags = ["moor", "moo", "lambdamoo", "rust", "server", "daemon", "kernel", "database", "transactions", "compiler", "virtual machine", "scheduler", "zeromq", "flatbuffers", "objdef", "architecture", "ground truth", "var", "schema", "hosts", "workers", "tasks", "permissions", "rpc protocol", "textdump", "build", "run", "test"]
children = "auto"
related = ["torchship"]
version = 2
---

# mooR server

This skill is written in ASD-STE100 Simplified Technical English.

Ground truth for the mooR codebase in `workspace/moor`. mooR is a network-accessible
virtual machine for shared, persistent, user-authored programs. It re-implements the
LambdaMOO server in Rust, keeps compatibility with LambdaMOO 1.8.x databases, and
replaces the parts of that design that do not scale.

Read the topic that matches the area, then the leaf skill for the exact subsystem.

## What the system is, in five sentences

A world is a database of permissioned objects. Each object holds properties and
verbs, and inherits both from one parent. A verb is a program that authors write and
compile from inside the running world, and the compiled form is stored in the
database with everything else. A task executes one verb call, inside one database
transaction. Participants reach the world through separate host processes; the
daemon process holds the database and runs the tasks.

## The four ideas that decide everything else

1. **A task is a transaction.** The unit of execution is also the unit of isolation.
   Where a task begins and ends decides what another task can see, what a retry
   re-runs, and what survives a failure. Most correctness questions in this codebase
   reduce to this one.
2. **Concurrency is optimistic, not locked.** LambdaMOO gave every task the whole
   database under one lock. mooR gives each task a consistent snapshot and checks for
   conflict at commit. This buys multicore throughput and it costs determinism: a
   task can be asked to run again. The level is snapshot isolation with write-write
   conflict detection, not full serializability. Parts of the book say otherwise.
   Read `moor/storage-and-state/transactions` before you rely on either word.
3. **Authority is in the database, not in the process.** Objects, verbs, and
   properties each carry an owner and permission bits. The engine enforces them. A
   process boundary is not a security boundary here; an object flag is.
4. **The world is authored live, and the source lives in the world.** The compiler,
   the decompiler, and the storage format all exist to keep authored source readable,
   editable, and reversible while the world runs. This constrains what may be changed
   in the language and in the stored program format.

## Topics

Reach for each umbrella by what you are about to touch, not by what it contains —
each one then points down to its own leaves.

- [storage-and-state](skill:moor/storage-and-state) — changing how objects,
  properties or verbs are stored, or anything about transactions and conflict.
- [language-and-compiler](skill:moor/language-and-compiler) — changing the MOO
  language itself: parsing, compiling, decompiling, the value model, or opcodes.
- [execution](skill:moor/execution) — changing how a verb runs: the scheduler, the
  VM, a builtin function, permission checks, or command parsing.
- [services](skill:moor/services) — changing anything outside the daemon: RPC,
  the wire schema, hosts, workers, the event log, the MCP host, or the clients.
- [content-pipeline](skill:moor/content-pipeline) — moving a world into or out of
  the server: objdef, textdump, or the bundled cores.
- [working-in-the-repo](skill:moor/working-in-the-repo) — building, running,
  testing, profiling, shipping, or following the project's own conventions.

## References

- **`references/crate-map.md`** — Every crate, what it owns, and which layer it is
  in. Read it before you decide where new code belongs.
- **`references/glossary.md`** — The vocabulary. Many words name a database concept,
  a language concept, and a process at once. Read it before your first change.
- **`references/doc-drift.md`** — Every place the book, the design notes, or the
  repository's own rules were found to disagree with the code. Read the row before
  you trust a document on that subject.

## Reading order

| You are | Read, in this order |
|---|---|
| New to the codebase | `references/glossary.md`, `references/crate-map.md`, then `moor/working-in-the-repo` |
| New to MOO itself | The book chapters on the database and the language, then the glossary |
| Changing engine behaviour | `moor/storage-and-state/transactions`, then `moor/execution/task-scheduler` |
| Changing the language | `moor/language-and-compiler`, all of it |
| Changing the protocol | `moor/services/wire-schema` before `moor/services/daemon-and-rpc` |
| Importing or exporting a world | `moor/content-pipeline` |

## Knowledge barriers

Each of these must be understood before the code in that area makes sense. None of
them is learned from the code alone.

| Barrier | Where it is taught |
|---|---|
| Multi-version concurrency, and snapshot isolation as distinct from serializability | `moor/storage-and-state/transactions` |
| Prototype inheritance, and why it is not class inheritance | `references/glossary.md`, `moor/storage-and-state/world-state-model` |
| That the same word names a value, a database entity, and a process | `references/glossary.md` |
| Bytecode interpretation, frames and unwinding | `moor/execution/virtual-machine` |
| Schema evolution against data already on disk | `moor/services/wire-schema` |
| What LambdaMOO compatibility does and does not promise | `moor/language-and-compiler/language-features-and-compat`, `moor/content-pipeline/textdump-compat` |

## Conventions in these bodies

A bare name in backticks, such as `transactions`, is a sibling skill in the same
topic. A name with slashes, such as `moor/execution/task-scheduler`, is a skill in
another topic. Find any of them with `skill_search`.

These bodies name crates, modules, types and config keys. They do not reproduce
source. Code moves; the skill would rot. When a body names a path, confirm it before
you depend on it.

## Where these bodies are not the authority

| Fact | Ask this instead |
|---|---|
| The crate list and dependency versions | The workspace root `Cargo.toml` |
| The current set of builtin functions | The generated builtin documentation, and the builtin modules |
| Configuration keys and defaults | The `--help` output of the binary, and its args module |
| The wire message set | The FlatBuffers schema sources in `crates/schema` |
| What the server requires of a database | The server's own startup checks |
| Test counts, opcode counts, any other count | Run the thing |

The `book/` directory is the project's manual and the best introduction to MOO. It
is a living document, and parts of it describe an earlier crate layout or an earlier
behaviour. The same is true of the design notes in `doc/`, of `AGENTS.md`, and of
several crate READMEs. Where a document and the code disagree, the code is correct.
`references/doc-drift.md` lists every disagreement found so far, with what the code
actually does. Each is also stated in the skill that owns the subject.

## Boundary with Torchship

The `torchship` skills describe one world that runs *on* this server: its objects,
its verbs, and its game systems. This library describes the server itself. If the
question is "what does this game do", read `torchship`. If the question is "what does
the engine do", read here. `torchship/torchship-programming/moor-book` teaches MOO as
a language for writing verbs; `moor/language-and-compiler` teaches how the engine
defines and implements that language.
