---
name = "The mooR task scheduler"
brief = "How a MOO task starts, runs, suspends, forks, retries after a conflict, and dies; the queues behind it and the tick and seconds limits that bound it."
when_to_use = "Use when the question is about task lifecycle in the mooR server: what starts a task, suspend and resume, background tasks, conflict retry and backoff, surviving a restart, or the tick/seconds limits on $server_options. Not for opcode execution, writing a builtin, permission checks, or command parsing, and not for Torchship or Thetis's internals."
universal = false
tags = ["moor", "scheduler", "task", "suspend", "fork", "resume", "kill_task", "ticks", "server_options", "conflict retry", "backoff", "background task", "checkpoint", "dump_interval", "queued_tasks", "task_recv", "task_send", "wait_task", "task ids", "fg_ticks", "bg_ticks", "fg_seconds", "bg_seconds", "max_stack_depth", "max_task_retries", "read()"]
version = 2
---

# The mooR task scheduler

The scheduler owns every task in the server. It decides when a task starts, on
which thread it runs, when a suspended task wakes, and what happens to its
transaction when it ends. It does not execute MOO code; it hands a prepared
`Task` to a thread pool worker, and the worker drives the VM.

Read `moor/execution` first for the task and transaction rule. This skill assumes it.

## What starts a task

A task begins as a `TaskStart` value. The variant decides how the VM is set up.

| Variant | Started by | Notes |
|---|---|---|
| `StartCommandVerb` | A line of player input | Parses the command inside the task's own transaction. |
| `StartDoCommand` | The same input, when `$do_command` exists | If `$do_command` returns a false value the same task rewrites itself to `StartCommandVerb` and parses, in the same transaction. |
| `StartVerb` | RPC verb invocation, login, connect and disconnect handlers, `$handle_task_timeout` | |
| `StartFork` | The `fork` statement | The only variant that counts as a background task. |
| `StartEval` | `eval` and the `;` command | A non-programmer's program is replaced with one that returns `E_PERM`. |
| `StartExceptionHandler` | An uncaught exception, when `$handle_uncaught_error` exists | |
| `StartBatchWorldState` | Host-side bulk read and write | Bypasses the VM entirely; runs world state actions in one transaction. |

Outside callers reach these through `SchedulerClient` in
`crates/kernel/src/tasks/scheduler_client.rs`. Requests are queued and answered
with a timeout, so a wedged scheduler produces `SchedulerNotResponding` rather
than a hang.

## Concurrency

Tasks run in parallel on a fixed thread pool sized from the logical core count.
Unlike LambdaMOO there is no global interpreter turn; isolation comes from the
per-task transaction, not from serialisation.

Three service threads sit beside the pool:

| Thread | Job |
|---|---|
| timer | Wakes expired suspensions, drains immediate wakes, triggers GC and periodic tasks-database compaction. |
| client requests | Runs requests submitted through `SchedulerClient`. |
| worker responses | Wakes tasks that were waiting on an out-of-process worker. |

All mutable scheduler state sits behind one lifecycle mutex. Task membership is
also mirrored into a lock-free registry so cheap existence checks do not take the
lock. A panicking task does not kill its pool thread; the pool catches it and logs.

Thread affinity is optional and configured by `runtime.task_pool_pinning` and
`runtime.service_perf_cores` in the daemon config.

## Active and suspended

A task is in exactly one of two places.

**Active** means a pool worker owns the `Task` and the VM may be running. The
scheduler holds only a control record: the player, the kill switch, the session,
and the result channel. Because the stack is moving, introspection of an active
task is limited to metadata. `active_tasks()` and `task_telemetry()` report it.

**Suspended** means the scheduler owns the whole `Task`, including its VM state.
It is inspectable, so `queued_tasks()` can report the verb and line number.

Each suspended task carries one wake condition:

| Wake condition | Set by |
|---|---|
| `Never` | `suspend()` with no argument |
| `Time` | `suspend(n)`, and a delayed `fork` |
| `Input` | `read()` |
| `Task` | `wait_task()` |
| `Immediate` | `commit()`, `suspend_if_needed()` past its threshold, `task_recv()` with no wait, and a brand new task before its first run |
| `Worker` | `worker_request()` |
| `GCComplete` | A task submitted while a GC sweep is in progress |
| `Retry` | Transaction conflict backoff |
| `TaskMessage` | `task_recv(seconds)` with an empty mailbox |

Timed wakes use a hierarchical hash wheel, so inserting and expiring a deadline
is cheap regardless of how many tasks are queued. Each suspension carries a
generation stamp, so a stale timer entry left by an earlier suspension of the
same task is ignored.

## Suspension is a commit

Every suspension commits first. The order matters and is fixed:

1. The VM yields with a suspend request.
2. The world state transaction commits. A conflict here turns the suspension into
   a retry instead.
3. The VM state snapshot is refreshed. This becomes the new retry point.
4. The session commits, publishing buffered narrative output. This happens
   outside the scheduler lock, because it does I/O.
5. The task moves from the active map to the suspension queue.

A session commit that fails does not roll back the world state. Output is treated
as non-critical; the world state's integrity wins.

`fork` follows the same path but resumes immediately in a new transaction rather
than suspending. The forked child gets a copy of the parent's activation, its own
task id, its own session forked from the parent's, and background limits.

## Failure and retry

| Outcome | What the scheduler does |
|---|---|
| Conflict on commit | Roll back the session, restore the VM snapshot, wait a randomised exponential backoff, run again in a new transaction. |
| Retries exceed `max_task_retries` | Report `TaskAbortedError` to the waiting caller and drop the task. |
| Tick or seconds limit | Commit or roll back per `rollback_on_task_limit`, send the player an abort message, report `TaskAbortedLimit`, and start `$handle_task_timeout` as a separate task with the resource name, stack, and backtrace. |
| Uncaught exception | Try `$handle_uncaught_error` on `#0` first. If it is absent or returns a false value, commit, log the backtrace, and report `TaskAbortedException`. |
| `kill_task()` | Set the kill switch. The task checks it between VM dispatch rounds, so the kill is best effort for an active task and immediate for a suspended one. The transaction rolls back. |
| Explicit `rollback()` | Roll back the world state; discard or publish the session as the argument asks. |
| Transaction renewal failure after a commit | Abort the task; the server cannot continue it. |

Backoff is a random base of tens of milliseconds shifted left by the retry count,
capped. It is deliberately jittered so that two tasks fighting over the same
relation do not resynchronise.

## The limits that bound a task

Defaults are constants in `crates/kernel/src/tasks/mod.rs`. The database overrides
them through properties on `$server_options`, reloaded by `load_server_options()`
and at startup. Read the current defaults from that file rather than from here.

| Property | Bounds |
|---|---|
| `fg_ticks`, `fg_seconds` | Any task that is not a fork. |
| `bg_ticks`, `bg_seconds` | Forked tasks. |
| `max_stack_depth` | Nested verb calls. Fixed when the task is created; a suspended task keeps the value it started with. |
| `max_task_retries` | Conflict retries before `TaskAbortedError`. |
| `max_task_mailbox` | Messages a task's `task_recv` mailbox may hold. |
| `rollback_on_task_limit` | Whether a tick or seconds limit rolls back rather than commits. |

`$server_options.dump_interval` and `$server_options.gc_interval` are read from
`#0`, not from the options object. A command-line setting for the checkpoint or GC
interval takes precedence over the database value.

The seconds limit is not checked on every tick. The VM host checks elapsed time
every sixty-fourth tick, so a single very slow opcode can overshoot.

What a verb author sees: the task stops where it stood, the player gets an
`Abort: Task exceeded ticks limit ...` or `Abort: Task exceeded time limit ...`
system message, and `$handle_task_timeout` runs separately. Exceeding
`max_stack_depth` is different — it raises `E_MAXREC` inside the task, which
ordinary MOO code can catch.

## Surviving a restart

Suspended tasks persist only when the `persistent_tasks` feature is on. The daemon
then supplies a real tasks database; otherwise it supplies a no-op one and every
suspended task is lost at shutdown.

On startup the scheduler loads every stored suspended task, gives each a fresh
background session, and re-arms its wake condition. Deadlines already in the past
wake immediately. No filtering is applied for age or for disconnected players, so
a long downtime produces a burst of waking tasks. The next task id is advanced
past the highest restored id.

Active tasks are never persisted. A crash loses them, and their transactions with
them.

## Checkpoints are not task state

`checkpoint()` and `dump_interval` write an objdef export of the **database**, not
of the task queue. The export runs against a database snapshot on a separate
thread, writes to an `.in-progress` file, and renames it to `.moo` on success.
Only one checkpoint runs at a time; a duplicate request is skipped with a warning.
Suspended tasks are written to the tasks database instead, continuously as they
suspend and again at shutdown.

## Invariants

1. **A running task always has a task context.** The thread-local context holds
   the transaction, the session, the task id, and the player. Kernel code that
   touches the world state outside one panics. Do not add a code path that reaches
   `with_current_transaction` from a thread that is not running a task.
2. **The world state commits before the session.** Never publish output for work
   that has not committed.
3. **A retry restores the last snapshot, not the start.** Any state you add to
   `Task` that must survive a retry has to be inside that snapshot or outside the
   VM entirely.
4. **Task ids are monotonic and never reused within a run.** Restart restores the
   counter above the highest persisted id.
5. **A task appears in exactly one of the active map and the suspension queue.**
   The transition is done under one lock acquisition, with an intermediate phase
   marker so a concurrent kill or shutdown sees a consistent state.
6. **Buffered inter-task messages are transactional.** `task_send()` holds
   messages until the sender commits, and discards them on rollback or retry.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| `SchedulerNotResponding` from an RPC call | The scheduler is not in the running state, or a client request timed out | Check startup order and shutdown state. Do not raise the timeout as a first move. |
| A forked task never runs after a restart | `persistent_tasks` is off | Turn the feature on, or accept the loss. |
| Log: "Task not found for suspend request" | The task was killed while it was committing its suspension | Usually benign. If frequent, look for a kill racing the suspension path. |
| Log: "No workers configured for scheduler" | `worker_request()` with no worker process attached | Start the worker. See `moor/services/workers`. |
| A burst of task starts after a restart | Restored deadlines that expired during downtime | Expected. There is no age filter by design. |
| `queued_tasks()` shows a task that never wakes | Wake condition `Never`, or a `Task` dependency on an id that no longer exists | Resume or kill it explicitly. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/kernel/src/tasks/mod.rs` | `TaskStart`, `ServerOptions`, limit defaults, `TaskHandle`. |
| `crates/kernel/src/tasks/task.rs` | The per-task loop, and every commit and rollback decision. |
| `crates/kernel/src/tasks/scheduler/mod.rs` | Scheduler state, the timer loop, service threads. |
| `crates/kernel/src/tasks/scheduler/scheduler_submit.rs` | Task submission entry points. |
| `crates/kernel/src/tasks/scheduler/scheduler_task_callbacks.rs` | Everything a running task reports back: success, exception, suspend, fork, retry, limits. |
| `crates/kernel/src/tasks/scheduler/scheduler_config.rs` | Reading `$server_options`. |
| `crates/kernel/src/tasks/task_q.rs` | The active map, the suspension queue, the timer wheel, mailboxes. |
| `crates/kernel/src/tasks/task_pool.rs` | The worker threads and their affinity. |
| `crates/kernel/src/tasks/tasks_db.rs` | The persistence trait; the daemon supplies the implementation. |
| `crates/kernel/src/tasks/checkpoint.rs` | Database checkpoint export. |
| `crates/kernel/src/tasks/task_scheduler_client.rs` | The handle a running task uses to talk back to the scheduler. |
| `crates/kernel/src/task_context.rs` | The thread-local transaction and session for the running task. |

## Where the book is behind the code

`book/src/the-system/moo-tasks.md` says a conflict re-executes the whole task
from the beginning. The code restores the snapshot taken at the last successful
commit or suspension, so work before that point is not replayed.

`book/src/the-system/controlling-the-execution-of-tasks.md` says values of
`fg_ticks` and `bg_ticks` below 100, and of `fg_seconds` and `bg_seconds` below 1,
are ignored. The loader accepts any non-negative value. There is no minimum.

The same chapter documents a `queued_task_limit` on the programmer or on
`$server_options`, raising `E_QUOTA` on `fork` or `suspend()`. No such limit
exists in the code.

## Read first / read next

Read `moor/storage-and-state/transactions` before you change any commit or
rollback path. Read `virtual-machine` for what the VM yields to this loop. Read
`moor/services/workers` for `worker_request()`, and `moor/services/hosts-and-sessions`
for what a session commit actually publishes.
