---
name = "The mooR object store"
brief = "Pick the right part of mooR's transactional object database: transactions and conflict retry, the object model, the storage engine, and object lifetime."
when_to_use = "Use for work in mooR's database layer: commit conflicts, relations, property and verb resolution, caches, snapshots, checkpoints, or garbage collection. Use it to choose the child skill. Not for MOO language or compiler questions, the scheduler's own queues and limits, the objdef or textdump file formats, the Torchship game database, or Thetis's own internals."
universal = false
tags = ["moor", "moo", "database", "transactions", "worldstate", "loaderinterface", "moor-db", "mvcc", "conflict", "commitresult::conflictretry", "retry", "objects", "properties", "verbs", "anonymous objects", "fjall", "snapshot", "checkpoint", "garbage collection", "storage"]
children = "auto"
related = ["moor/execution/task-scheduler", "moor/execution/permissions-and-security"]
version = 2
---

# The mooR object store

This skill is written in ASD-STE100 Simplified Technical English.

The `moor-db` crate in `crates/db` holds the world: every object, property, verb
program, and permission bit. It is a small purpose-built database, not a wrapper on
someone else's. It gives each task a consistent view of the world, detects conflict
at commit, and writes the result to a key-value store on disk.

Read this page to choose a child. Do not act from this page alone.

## Which child to read

- [transactions](skill:moor/storage-and-state/transactions) — why a commit
  conflicts, and what a retry re-runs.
- [world-state-model](skill:moor/storage-and-state/world-state-model) — how
  objects, properties, verbs and inheritance are stored, and who checks
  permissions.
- [storage-engine](skill:moor/storage-and-state/storage-engine) — what is in
  memory, what is on disk, what survives a crash, and what a snapshot,
  checkpoint or migration is.
- [object-lifecycle-and-gc](skill:moor/storage-and-state/object-lifecycle-and-gc)
  — how an object is created, recycled or collected, and whether it is safe
  to hold a reference across a transaction.

## Common to all four

**One crate, four layers.** Each layer has one job, and each hides the layer below.

| Layer | Path | Owns |
|---|---|---|
| Public handle | `crates/db/src/lib.rs` | `TxDB`, the `Database` trait, opening the database |
| API adapters | `crates/db/src/api` | `DbWorldState`, permission checks, the loader and GC adapters |
| Engine | `crates/db/src/engine` | `MoorDB`, the relation set, the transaction type, the commit pipeline |
| Transaction machinery | `crates/db/src/tx` | Per-relation transactions, indexes, conflict check, resolvers |
| Providers | `crates/db/src/provider` | Encoding to and from the on-disk key-value store |
| Caches | `crates/db/src/cache` | Verb, property and ancestry resolution caches |

**One word, one meaning.** These four skills use these terms as follows.

| Term | Meaning here |
|---|---|
| Relation | One typed key-to-value map, such as `object_parent`. The unit of storage and of conflict. |
| Domain, codomain | The key and the value of a relation. |
| Working set | The mutations one transaction made, per relation, held until commit. |
| Root snapshot | The published in-memory version of every relation index. Readers start from one. |
| Snapshot interface | A read-only on-disk view used for export. A different thing from the root snapshot. |
| Transaction abort | The database refused the commit. The caller must run again. |
| Task abort | The scheduler stopped a task. See `moor/execution/task-scheduler`. |

**Two calls, one word.** `checkpoint` on the scheduler writes an objdef export to a
directory. `TxDB::checkpoint` is a separate trait method and currently does nothing.
Neither one forces the world state to disk. `storage-engine` explains this.

**Everything is one process.** The database is opened by the daemon and lives in that
process. There is no database server, no network hop, and no second writer. A second
process that opens the same directory is a fault, not a feature.

## Knowledge barriers

Do not change this area before you understand these. The code does not teach them.

| You must understand | Learn it from |
|---|---|
| Multi-version concurrency: readers on a snapshot, writers checked at commit | `transactions`, first two sections |
| Snapshot isolation, and how it differs from serializable | `transactions`, and note that the book calls it both things |
| The MOO object model: one parent, properties defined once and overridden, verbs resolved by ancestry | `world-state-model`, and `book/src/the-database/objects-in-the-moo-database.md` |
| The difference between a transaction abort and a task abort | `transactions`, then `moor/execution/task-scheduler` |
| Who owns a permission decision | `world-state-model`, then `moor/execution/permissions-and-security` |
| Why an object number is never reused | `object-lifecycle-and-gc` |

## The rule that holds across all four

A task is a transaction. Where a task begins and ends decides what it sees, what a
retry repeats, and what survives failure. If a change moves a transaction boundary,
it changes the semantics of the whole server, not only the database. Treat any such
change as a design change and read `moor/execution/task-scheduler` first.

## Where the book is behind the code

The book in `book/src` is the project's manual and a good introduction. Two places
disagree with the code, and the code is correct.

| Book statement | The code |
|---|---|
| `book/src/the-system/performance-and-concurrency.md` says the database has "serializable isolation" | No read set is tracked. Only written keys are checked. This is snapshot isolation. `book/src/the-database/transactions.md` says so correctly. |
| `book/src/the-database/transactions.md` says a conflicting command "automatically retries from the beginning" | A retry restores the task's virtual machine from the last transaction boundary. Only a task that never crossed one starts again from the beginning. |
| `book/src/the-system/server-assumptions-about-the-database.md` says restarting "will always restore the system to the state it was in" | Nothing in the database crate requests an fsync for world state. A clean stop drains the writer; an unclean stop can lose recent commits. |
| The same chapter lists textdump as one of two checkpoint formats | A checkpoint always writes objdef. The export-format option is deprecated and ignored, and says so in its own help text. |

## Read next

- `moor/execution/task-scheduler` — who starts and ends a transaction, and who runs a
  retry.
- `moor/execution/permissions-and-security` — the model that `DbWorldState` enforces.
- `moor/content-pipeline/objdef-format` — what a checkpoint writes and an import reads.
- `moor/working-in-the-repo/testing` — the database tests, including the history
  checker and the concurrency model checker.
