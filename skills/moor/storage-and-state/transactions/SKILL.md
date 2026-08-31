---
name = "Transactions and conflict retry"
brief = "How a mooR transaction starts, what it reads, how commit detects conflict, and what a retry re-runs — plus what a retry does not undo."
when_to_use = "Use when a commit conflicts, a task retries or reports 'Transaction conflict', or you must reason about isolation, lost updates, or transaction boundaries in the mooR daemon. Not for the scheduler's queues, ticks and suspend rules (read moor/execution/task-scheduler), and not for the Torchship database."
universal = false
tags = ["moor", "transactions", "mvcc", "conflict", "retry", "commitresult", "conflictretry", "commit", "commit pipeline", "isolation", "visibility", "snapshot isolation", "optimistic concurrency", "rollback", "moor-db", "write skew", "lost update"]
version = 2
---

# Transactions and conflict retry

Every task in mooR runs inside one database transaction. The transaction gives the
task a fixed view of the world, collects the task's writes in memory, and checks
them against the current world only at commit. This page explains the model, the
lifecycle, and the costs a verb author and an operator pay for it.

## Why the global lock went away

LambdaMOO ran one task at a time. That made every task trivially isolated and made
the whole server single-threaded. One slow verb stopped the world.

mooR keeps the isolation and drops the lock. Each task reads from a published,
immutable snapshot of the world. Writes go to a private working set. At commit the
database asks one question: did another transaction change any key that I am writing,
after I read it? If no, the commit publishes. If yes, the commit is refused and the
task runs again.

The trade is explicit:

- **Bought:** tasks run in parallel on many cores. A slow task blocks only itself.
- **Paid:** a task can be asked to run twice. Work outside the database does not get
  a second chance to be undone. Some correct-looking MOO code is now wrong.

## The isolation level you actually get

mooR gives **snapshot isolation with write-write conflict detection**. It does not
track read sets. A transaction records an entry in its working set only when it
mutates a key. Reads are served from the snapshot index and leave no trace.

Consequences to state plainly:

- No dirty reads. No lost updates on the same key.
- **Write skew is possible.** Two transactions may read the same keys, each decide
  based on the other's old value, and each write a *different* key. Both commit. The
  invariant they were both defending is now broken.
- The fix in MOO code is to force an overlap: write the value you depended on, or
  keep the related values in one property so that they share one key.
- Real-time order is not guaranteed. A commit that happened later in wall-clock time
  may be ordered earlier.

`book/src/the-system/performance-and-concurrency.md` calls this "serializable
isolation". That is wrong. `book/src/the-database/transactions.md` describes it
correctly. Believe the code.

## The lifecycle

| Stage | What happens |
|---|---|
| Start | `TxDB::new_world_state` asks `MoorDB` for a transaction seed. |
| Seed | The seed takes the published root snapshot, a fresh monotonic write timestamp, the shared sequence counters, and forked copies of the resolution caches. |
| Read | Every read is served from the snapshot's immutable relation indexes, then from the transaction's own pending writes if it has any. The snapshot never changes under the task. |
| Write | Each mutation is stored in a per-relation working set, with the timestamp of the value that was read and the transaction's own write timestamp. |
| Commit | The working sets go to the commit pipeline. See below. |
| Rollback | The transaction is dropped. Nothing was published, so nothing must be undone. |

Two timestamps matter. `visible_ts` is the committed timestamp of the snapshot the
transaction started from; it decides visibility. `ts` is the transaction's own write
timestamp, taken from a monotonic counter at start. Per key, `read_ts` records the
timestamp of the value the transaction saw.

## How conflict is detected

For each key in each non-empty working set, the check compares the key's current
canonical state against what the transaction saw:

| Situation | Conflict type |
|---|---|
| We insert, the key now exists | `InsertDuplicate` |
| We update, the canonical timestamp is newer than our `read_ts` | `ConcurrentWrite` |
| Our `read_ts` is newer than our write timestamp | `StaleRead` |
| We update, the key no longer exists | `UpdateNonExistent` |

Before it fails, the check tries to resolve:

1. **Identical write.** If the other transaction wrote exactly the value we want, or
   if we delete a key that is already gone, there is no real conflict. Accept.
2. **Smart merge.** For a `Var` codomain, `RelationCodomain::try_merge` attempts a
   three-way merge of base, theirs and mine. It only handles two shapes, and only
   when both sides carried the matching operation hint: a single map insert of
   different keys, and a single flyweight slot insert of different slots. Everything
   else returns no merge.
3. Otherwise the commit fails with a `ConflictInfo` that names the relation, the key,
   and the conflict type. For a property relation the pipeline fills in the property
   name before returning.

Keys created by object creation are marked `guaranteed_unique` and skip the check.
That is safe only because their identifiers come from a shared allocator.

## The commit pipeline

`crates/db/src/engine/moor_db/commit_pipeline.rs` runs this, in order:

1. **Read-only fast path.** A transaction with no mutations publishes only its
   resolution-cache updates and returns success. It can never conflict.
2. **Check.** If the root snapshot has not advanced since the transaction started,
   the check is skipped. If it has advanced but the root's cumulative bloom filter of
   recently written keys does not intersect this transaction's keys, the check is
   also skipped. Otherwise every key is checked.
3. **Prepare.** The new relation indexes are built from the working sets, and a bloom
   filter of this commit's keys is produced. Nothing is published yet.
4. **Publish.** A compare-and-swap on the root pointer. It succeeds only if the root
   is still the version that was checked.
5. **Rebase.** If another writer won the compare-and-swap, the pipeline compares its
   keys against the winner. If they are provably disjoint — by bloom filter, or by
   exact key comparison of the two snapshots — it re-slots the prepared indexes onto
   the winner and tries again, up to `MAX_REBASE_ATTEMPTS`. A real key overlap ends in
   a conflict. Exhausting the attempts also ends in a conflict, with no detail.
6. **Persist.** Only after a successful publish is the batch handed to the background
   writer. See `storage-engine`.

Only step 4 serializes. Checking and preparing run in parallel across workers.

## What a retry re-executes

A refused commit returns `CommitResult::ConflictRetry`. It is not an error value;
the caller must act on it.

The scheduler's retry path is in
`crates/kernel/src/tasks/scheduler/scheduler_task_callbacks.rs`:

1. The task's session output is rolled back, so the player sees no duplicate text.
2. The task's retry counter increases. If it passes `max_task_retries` from
   `$server_options`, the task aborts with a task error instead.
3. The task suspends for a randomized backoff that doubles with each retry, with a
   capped exponent.
4. On wake the task gets a **new transaction** and its VM state is restored from
   `retry_state`.

`retry_state` is a snapshot of the VM taken at the last transaction boundary. A task
that never suspended and never forked has its initial state there, so the whole verb
runs again from the first instruction. A task that already crossed a boundary resumes
from that boundary, not from the start.

Retry tasks are never persisted across a server restart.

## What a retry does not undo

This is the part that surprises people. Rolling back the transaction does not roll
back the world.

| Not undone | Why |
|---|---|
| Sequence numbers, including the maximum object number | Sequences live on the shared database handle, not in the working set. A failed transaction still consumed the numbers it allocated. Object numbers are skipped, never reused. |
| Work sent to an out-of-process worker | The request already left the daemon. |
| Anything a builtin did outside the database | The transaction has no record of it. |
| Time | A verb that measures elapsed time sees the retry's clock, not the first attempt's. |

Session output *is* buffered and is rolled back. That is deliberate, and it is why a
retried command does not print twice.

## Two commits you might not expect

Read these before you decide that "the transaction rolls back on failure".

- **An uncaught MOO exception still commits.** The task reports the traceback after
  committing. LambdaMOO behaved this way and mooR keeps it.
- **A task that exceeds its tick or second limit commits by default.** The
  `rollback_on_task_limit` server option changes this to a rollback. It defaults to
  false.

## Transaction abort versus task abort

Do not confuse them.

| | Transaction abort | Task abort |
|---|---|---|
| Who decides | The database, at commit | The scheduler or the VM |
| Signal | `CommitResult::ConflictRetry` | A `SchedulerError` variant |
| Effect on world state | Nothing was published | Depends; may have already committed |
| Effect on the task | It runs again | It ends |
| Visible to MOO code | No | Yes, as an aborted task |

## Invariants

1. **A transaction reads one snapshot for its whole life.** Never let a read reach
   the live root. If you do, a task can observe another task's commit mid-execution,
   and the conflict check will not protect it.
2. **A key that a transaction writes must appear in that relation's working set.** A
   write that bypasses the working set is invisible to the check, and silently
   overwrites concurrent work.
3. **Publication is a compare-and-swap on the root, and nothing else publishes.**
   Any other path that mutates a published index breaks isolation for every reader
   that already holds it.
4. **Persistence happens only after publication, and in version order.** Writing
   before publication would make a rejected transaction durable.
5. **Conflict resolution must be a function of base, theirs and mine only.** A
   resolver that reads anything else is not reproducible under retry.
6. **A retry must be safe to run again.** Any code path that commits must be
   re-entrant from `retry_state`.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| "Task retry limit exhausted; aborting task" in the log | Sustained contention on one key | Read the `ConflictInfo` in the log line; it names the relation and key. Reduce the write to that key, or split the value. |
| Steady low-level retries under load | Many tasks writing one hot object or property | Application shape, not a server fault. Split the hot value across keys. |
| Two tasks each read the other's state and both commit wrongly | Write skew; no read set is tracked | Make one of them write the key it depended on. |
| Commit succeeds but the change is missing after a restart | The write never reached disk | Read `storage-engine`. Check for batch-writer errors in the log. |
| A conflict with no `ConflictInfo` | The rebase attempts ran out | Rare. Treat as heavy write contention. |
| Object numbers have gaps | Retried or failed transactions consumed sequence values | Expected. Do not "fix" it. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/db/src/tx/transaction.rs` | The per-relation transaction, its local operations, and the working set |
| `crates/db/src/tx/check.rs` | Conflict detection and the three-way merge attempt |
| `crates/db/src/tx/resolve.rs` | The resolver strategies: fail, accept-identical, smart merge |
| `crates/db/src/tx/commit_bloom.rs` | The bloom filter used to skip checks and prove disjointness |
| `crates/db/src/engine/moor_db/commit_pipeline.rs` | The check, prepare, publish, rebase and persist stages |
| `crates/db/src/engine/moor_db/snapshot_planes.rs` | The published root and the cache sidecar |
| `crates/db/src/engine/relation_defs.rs` | The macro that generates the per-relation check, apply and rebase code |
| `crates/kernel/src/task_context.rs` | The thread-local transaction, and commit and rollback for a task |
| `crates/kernel/src/tasks/task.rs` | Every transaction boundary a task can cross |
| `crates/kernel/src/tasks/scheduler/scheduler_task_callbacks.rs` | The retry decision, the backoff, and the retry limit |
| `crates/db/tests/jepsen_history.rs` | Replays a generated history and asserts the expected outcomes |
| `crates/db/src/engine/moor_db_concurrent_tests.rs` | Schedule exploration with the model checker |

## Read first, read next

Read `moor/storage-and-state/world-state-model` to know which relation a given MOO
operation writes; that is what decides your conflict shape. Read `storage-engine` for
what happens after publication. Read `moor/execution/task-scheduler` for who owns the
transaction boundaries and the retry. Read `moor/working-in-the-repo/testing` before
you change the commit pipeline; the history and model-checker tests are the ones that
catch isolation faults.
