---
name = "The storage engine and durability"
brief = "What mooR keeps in memory versus on disk, how a commit reaches the fjall key-value store, what a snapshot and a checkpoint are, and what a crash costs."
when_to_use = "Use for questions about mooR's persistence: memory use and database size, the fjall keyspaces and on-disk format, the background batch writer and its backpressure, wait_for_persistence, fsync and durability guarantees, database snapshots, checkpoints and exports, startup time, database version migration, or what survives a crash or restart. Use it before changing encoding, the writer path, or DatabaseConfig. Not for conflict detection and retry, the relation model, or object lifetime, which the sibling skills own. Not for the event log or task database, the Torchship game database (the torchship skills own that), or Thetis's own internals."
universal = false
tags = ["moor", "database", "storage", "fjall", "durability", "fsync", "persistence", "batch writer", "snapshot", "checkpoint", "migration", "restart", "crash", "memory", "keyspace", "moor-db"]
version = 1
---

# The storage engine and durability

The world lives in memory. Disk is the backing store that lets the process start
again. This page states that division exactly, and states what mooR does and does not
promise about a crash.

## The one fact that shapes everything

**At open, every relation is scanned in full from disk into an in-memory index.** The
whole world is resident. Reads never go to disk in steady state.

Two consequences follow, and an operator must plan for both:

- Memory use tracks the world size, not the working set. A large world needs memory
  for all of it.
- Startup cost tracks the world size. A restart re-reads and re-indexes everything
  before it serves anything.

The indexes are persistent immutable maps from `imbl`. Forking one is cheap and
shares structure, which is what makes a per-transaction snapshot affordable.

## The layers

| Layer | Type | What it is |
|---|---|---|
| Root snapshot | `WorldStateSnapshot` | One version number, one committed timestamp, one immutable index per relation, plus the resolution caches and a bloom filter of recent writes |
| Publication planes | `SnapshotPlanes` | Two atomic pointers: the root, and a cache sidecar that read-only commits may update alone |
| Relation index | `RelationIndex` | The in-memory map for one relation, with an optional value-to-keys reverse map |
| Provider | `FjallProvider` | Encoding and decoding for one relation, and its fjall keyspace |
| Writer | `BatchWriter` | The one background thread that commits batches to fjall, in version order |
| Store | `fjall` | The log-structured key-value store on disk |

`fjall` is the only storage dependency. It is named in `crates/db/Cargo.toml`. One
fjall keyspace exists per relation, named after the relation, plus a `sequences`
keyspace for counters and markers.

## The on-disk form

A stored value is the encoded value followed by an eight-byte little-endian
timestamp. The timestamp is what makes conflict detection work after a restart: the
recovered index carries the committing transaction's timestamp for every tuple.

Encoding is per type, not universal:

- Fixed-size types are written with `zerocopy`, byte for byte.
- Values, verb definition sets, property definition sets and programs are written as
  FlatBuffers through `planus`.
- Byte-backed types pass through unchanged.

The `sequences` keyspace holds a format marker. A database whose marker does not
match is refused at open, rather than misread.

## Startup

| Step | Effect |
|---|---|
| Migration check | Runs before the database opens. If the version marker is behind, the whole directory is copied aside, migrated, and swapped in atomically. |
| Open | The fjall database and the `sequences` keyspace open. |
| Format check | A fresh database gets the marker. An existing one must match it. |
| Sequence load | Counters are read back. A fresh database starts them at -1. |
| Relation seeding | Every relation is scanned. The highest tuple timestamp becomes the recovered committed timestamp. |
| Timestamp restart | The monotonic transaction counter starts above the recovered committed timestamp. |

`try_open` returns a flag saying whether the database was fresh. The daemon imports a
world only when it is fresh; an existing database is never overwritten by an import.

## The write path after commit

Publication and durability are separate.

1. The commit pipeline publishes a new root. Readers see the change immediately.
2. Only then is the batch encoded and handed to the `BatchWriter`.
3. Batches may arrive out of order, because different worker threads win the
   publish. The writer holds them and commits only contiguous versions, so the
   on-disk state is always a prefix of the published history.
4. Each version becomes one cross-keyspace fjall write batch. Dirty sequence counters
   ride along in the same batch.
5. A completed-version counter advances. Barrier and snapshot waiters are released.

If encoding or enqueueing fails, the failure is fatal. The database does not continue
with a hole in its history; it raises a fatal database error.

## Durability: what is actually promised

Read this before you tell anyone that a commit is durable.

- **Nothing in the database crate requests an fsync for world state.** Search
  `crates/db/src` for a persist call and you find one only in a test.
- `TxDB::wait_for_persistence` waits for the batch writer to commit the currently
  published version into fjall. Its own documentation says this is not an fsync and
  not a flush. It is an application-level handoff.
- `TxDB::checkpoint` is a trait method that currently does nothing. It is marked as
  such in the source.
- A clean shutdown drains the writer. `MoorDB::stop` reports an error if the writer
  stopped before it reached the published version, and the `Drop` implementation calls
  it.

So: a **clean stop** keeps everything. A **process crash** loses whatever the writer
had not yet committed. A **machine crash** additionally depends on the store's own
journal flushing behaviour, which mooR does not override. If you need a stronger
promise, the honest answer is a checkpoint export, not a claim about the write path.

## Two things called a snapshot

Keep them apart.

| Name | What it is | Used for |
|---|---|---|
| Root snapshot | The published in-memory index set | Every transaction's read view |
| `SnapshotInterface` | A read-only cross-keyspace view of the on-disk store, taken after the writer has committed through a given version | Export and checkpoint |

`Database::create_snapshot` asks the batch writer for the second kind, waiting up to a
fixed timeout for the published version to be committed. `create_snapshot_async` does
the same on a separate thread and calls back. The returned reader decodes the
keyspaces directly, so it never touches a live transaction.

## What a checkpoint is

A checkpoint is an **export**, not a database flush.

1. The scheduler takes the checkpoint flag, so only one runs at a time.
2. It requires an output directory in the import and export configuration. Without
   one, the checkpoint fails and logs it.
3. It takes an asynchronous storage snapshot.
4. It collects object definitions and writes an objdef directory to a file named for
   the current time, with an in-progress extension.
5. On success the file is renamed to its final `.moo` name. A file left with the
   in-progress name means the export did not finish.

Blocking and non-blocking modes exist. The non-blocking one returns as soon as the
snapshot is requested. `moor/content-pipeline/objdef-format` owns the format of what
is written.

A checkpoint always writes objdef. The daemon still accepts an export-format option,
but it is deprecated, hidden and ignored, and it says so in its own help text. The
book still lists textdump as a checkpoint format; it is not one.

## Tuning and observation

| Knob or counter | Where | Notes |
|---|---|---|
| Per-relation memtable size | `DatabaseConfig` in `crates/db/src/config.rs` | Two relations get larger memtables by default because they take the most write pressure |
| Storage maintenance counters | `StorageMaintenanceStats` from `Database::storage_maintenance_stats` | Write buffer bytes, outstanding flushes, active compactions, journal size, disk size |
| Database counters | `db_counters()`, exposed to MOO as a wizard-only builtin | Commit phase timers, conflict counts, merge success and failure |
| Cache statistics | The cache statistics types in `crates/db/src/cache` | Hits, negative hits, misses per cache |

`DatabaseConfig` also names two settings, `object_contents` and `object_children`,
that no relation uses. Setting them does nothing.

Do not write current default values into code or documentation. Read them from
`crates/db/src/config.rs` and from the daemon's own argument help.

## Invariants

1. **The durable state is a prefix of the published history.** Versions are committed
   in order and never skipped. A change that lets a later version land first makes
   recovery incoherent.
2. **A tuple's stored timestamp is the timestamp of the transaction that wrote it.**
   Recovery depends on it. Do not rewrite a value without carrying its timestamp.
3. **Publication happens before persistence, never after.** Reversing this would make
   a rejected transaction durable.
4. **A failure in the write path is fatal, not recoverable.** Continuing after a lost
   batch would leave memory and disk permanently disagreeing.
5. **The format marker and the version marker are checked at open.** Never bypass
   them to "just open it". Migration exists for that.
6. **Migration works on a copy and swaps atomically.** A migration that edits the
   live directory can leave an unopenable database.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Slow batch commit warnings in the daemon log | Storage cannot keep up, or one value is very large | Check disk latency. The warning names the slowest target, including the property name. Split large property values. |
| Rising batch-writer backpressure counters | Sustained write rate above what the device can take | An operator problem, not a code problem. Faster storage, or fewer and smaller writes. |
| "FATAL" and a database error at commit | Encode or enqueue failure | The process signals a fatal database error. Do not restart into the same fault without reading the message. |
| Startup takes minutes | The full relation scan on a large world | Expected. Measure it before assuming a fault. |
| Open refused with a tuple value format error | The database was written by a different value format | Do not delete it. Find the version that wrote it. |
| Open refused with a migration error | The copy-and-swap did not complete | Look for the leftover migrating and old directories beside the database. |
| A commit is missing after an unclean stop | The writer had not committed it | Expected under the durability model. Use checkpoints for anything that must survive. |
| A checkpoint file keeps the in-progress extension | The export failed part way | Read the error in the log. The rename is the last step. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/db/src/lib.rs` | `TxDB`, the `Database` trait, open errors, maintenance statistics |
| `crates/db/src/engine/moor_db.rs` | Open, seed, sequences, shutdown, snapshot creation |
| `crates/db/src/engine/moor_db/snapshot_planes.rs` | The root pointer and the cache sidecar |
| `crates/db/src/provider/fjall_provider.rs` | Per-type encoding, decoding, and the keyspace handle |
| `crates/db/src/provider/batch_writer.rs` | The ordered background writer, barriers and snapshot waiters |
| `crates/db/src/provider/fjall_snapshot_loader.rs` | The read-only on-disk reader used for export |
| `crates/db/src/provider/fjall_migration.rs` | Version detection and the copy-and-swap migration |
| `crates/db/src/tx/indexes.rs` | The in-memory index implementations |
| `crates/db/src/config.rs` | `DatabaseConfig` and per-relation keyspace options |
| `crates/kernel/src/tasks/checkpoint.rs` | The checkpoint export |
| `crates/daemon/src/lib.rs` | Opening the database at startup and the first import |
| `crates/db/tests/save_restore.rs` | Writes a world, reopens it, and verifies it came back |

## Read first, read next

Read `transactions` first; publication order only makes sense once you know what a
commit publishes. Read `world-state-model` for what the relations mean. Read
`moor/content-pipeline/objdef-format` for the checkpoint output, and
`moor/content-pipeline/textdump-compat` for the other import path. Read
`moor/working-in-the-repo/build-and-run` for where the database directory sits in a
running deployment.
