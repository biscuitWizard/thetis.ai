---
name = "Running verbs in mooR"
brief = "The parts of the mooR server that execute MOO code: tasks, the scheduler, the bytecode VM, builtin functions, permissions, and the command parser."
when_to_use = "Use when a change or a question touches the running of MOO code in the mooR server: task suspend, fork, resume, kill, ticks and seconds limits, E_MAXREC, transaction conflict retry, the activation stack, adding or fixing a builtin function, wizard and programmer bits, set_task_perms and capability grants, or how a typed command becomes a verb call. Use it to pick which child skill to read. Do not use it for the database engine or the transaction protocol itself, for the MOO compiler and opcode set, or for the RPC and host layer. Do not use it for the Torchship MOO game database, for writing in-world MOO verbs for a specific game, or for Thetis's own internals."
universal = false
tags = ["moor", "execution", "task", "scheduler", "vm", "builtin", "permissions", "command parser", "suspend", "fork", "ticks", "wizard", "verb call"]
children = "auto"
version = 1
---

# Running verbs in mooR

Everything that executes MOO code lives in two crates. `crates/kernel` owns tasks,
the scheduler, the VM host, and the builtin functions. `crates/vm` owns the
interpreter itself: activations, frames, opcode execution, and unwinding.
`crates/common` owns the command parser and the permissions type they share.

This skill is a dispatch table. Read the child that matches your work.

## Which child to read

| Read | When you are working on |
|---|---|
| `task-scheduler` | Task lifecycle, queues, suspend, fork, resume, kill, retry after conflict, restart recovery, checkpoints, tick and time limits. |
| `virtual-machine` | The activation stack, verb call setup, opcode execution, error propagation and unwinding, the `d` flag, `E_MAXREC`. |
| `builtin-functions` | Adding, changing, or debugging a `bf_*` function; argument and error conventions; builtin documentation and tests. |
| `permissions-and-security` | Who a task runs as, the wizard and programmer bits, `set_task_perms`, capability grants, where a check is and is not applied. |
| `command-parsing` | Turning a line of typed input into a verb call: `$do_command`, prepositions, object matching, `:huh`. |

## The two facts common to every child

**1. A task is the unit of execution and the unit of transaction.**

One task holds exactly one open world state transaction at a time. Every property
read, property write, object creation, and verb program lookup that the task makes
goes through that transaction. Nothing the task writes is visible to any other task
until the transaction commits.

The task boundary and the transaction boundary are not the same. A task can span
several transactions in sequence. Each of these ends the current transaction and
starts a new one:

| Event | World state | Session output |
|---|---|---|
| Task returns normally | Commit | Publish |
| Task raises an uncaught exception | Commit | Publish |
| `suspend()`, `read()`, `commit()`, `suspend_if_needed()`, `task_recv()` | Commit, then begin a new one | Publish |
| `fork` statement dispatch | Commit, then begin a new one | Publish |
| Tick or seconds limit reached | Commit by default; roll back if `$server_options.rollback_on_task_limit` is true | Follows the world state |
| `rollback()` builtin | Roll back | Discard, unless the argument asks to keep it |
| `kill_task()` takes effect | Roll back | Discard |
| Commit conflicts with another task | Roll back, then retry | Discard |

Committing an exception is deliberate. LambdaMOO commits the work a failing command
already did, and mooR keeps that behaviour.

**2. A conflicting commit re-runs the task from its last commit point, not from
the start.**

Each task keeps a snapshot of its VM state, refreshed at every successful commit
and every suspend. A conflict restores that snapshot, backs off, and runs again in
a fresh transaction. A task that has already suspended once therefore does not
replay the work before the suspend. The retry count is bounded; see
`task-scheduler` for the limit and the failure it produces.

The consequence for a verb author: any side effect that is not in the world state
and not in the session buffer can happen more than once. A `fork` already
dispatched, a worker request already sent, or a file already written by a worker
survives a retry that discards the database work.

## The order of the pipeline

One line of player input passes through all five areas, in this order. Use it to
decide which child owns the stage you are looking at.

| Stage | Owner | Child |
|---|---|---|
| A host turns a connection line into a task submission | `crates/daemon`, the telnet and web hosts | `moor/services/hosts-and-sessions` |
| The scheduler assigns a task id, opens a transaction, and hands the task to a pool worker | `crates/kernel/src/tasks` | `task-scheduler` |
| `$do_command`, then the built-in parser, resolve the line to a verb and a target | `crates/kernel/src/tasks/task.rs`, `crates/common/src/matching` | `command-parsing` |
| The verb is resolved and an activation is pushed with the verb owner's authority | `crates/db`, `crates/vm` | `permissions-and-security`, `virtual-machine` |
| Opcodes run; builtins are called; errors raise or are pushed | `crates/vm`, `crates/kernel/src/vm` | `virtual-machine`, `builtin-functions` |
| The VM yields; the task commits, suspends, forks, or dies | `crates/kernel/src/tasks/task.rs` | `task-scheduler` |

## Numbers that drift

Do not trust a count or a default written in any skill in this topic. Get the
current value from the source of truth instead.

| Fact | Source of truth |
|---|---|
| Default ticks, seconds, stack depth, retries | The constants in `crates/kernel/src/tasks/mod.rs`, overridden by `$server_options`. |
| The live limits on a running server | `$server_options`, and `load_server_options()` to reload them. |
| The list of builtin functions and their arities | `function_info()` in world, or `crates/common/src/builtins.rs`. |
| A builtin's documentation | `function_help()` in world. It is generated from the source. |
| The opcode set | `moor/language-and-compiler/program-and-opcodes`. |
| The feature flags that change execution | `FeaturesConfig` in `crates/vm/src/config.rs`. |

## Knowledge barriers

Do not change anything in this area before you understand these, in this order:

1. **The transaction protocol.** What `CommitResult::ConflictRetry` means, what
   the isolation level guarantees, and which relations conflict.
   Read `moor/storage-and-state/transactions`.
2. **The world state interface.** How verbs and properties resolve through the
   inheritance chain, and which calls take a `TaskPermissions`.
   Read `moor/storage-and-state/world-state-model`.
3. **The program representation.** What a `Program` holds, what an opcode is, and
   why builtin ids are frozen. Read `moor/language-and-compiler/program-and-opcodes`.
4. **The value model.** `Var`, `Obj`, `Symbol`, `List`, `Error`, and the flyweight
   and lambda types. Read `moor/language-and-compiler/value-model`.

You do not need the RPC layer to change the VM. You do need it to change how a
task reaches the outside world; that is `moor/services/daemon-and-rpc` for hosts
and `moor/services/workers` for out-of-process work.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/kernel/src/tasks/` | The task, the scheduler, the queues, the tasks database, telemetry. |
| `crates/kernel/src/vm/` | The VM host, the builtin registry, all `bf_*` modules. |
| `crates/kernel/src/task_context.rs` | The thread-local transaction, session, and scheduler client for the running task. |
| `crates/vm/src/` | The interpreter: `ExecState`, `Activation`, `MooStackFrame`, `moo_execute`, `vm_unwind`. |
| `crates/common/src/matching/` | The command parser and the object name matchers. |
| `crates/common/src/model/task_permissions.rs` | `TaskPermissions` and `CapabilityGrant`. |
| `crates/common/src/builtins.rs` | The builtin id table. Shared by the compiler and the VM. |

## Hazards that cross all five children

| Symptom | Cause | Action |
|---|---|---|
| A verb's changes vanish, and the log says "Transaction conflict" | Two tasks wrote the same relation | Nothing to fix in most cases; the scheduler retries. If it repeats, reduce the write set or split the task at a `commit()`. |
| "Task retry limit exhausted" and `TaskAbortedError` reaches the caller | Retries exceeded `$server_options.max_task_retries` | A hot contended write. Split the task, or narrow what it writes. |
| A verb runs twice, and an external effect happens twice | A retry replayed work that had a non-transactional side effect | Move the side effect after the last commit point, or make it idempotent. |
| Panic: "Task has empty activation stack" | Kernel code reached the VM stack when no activation was pushed | A bug in the caller, not in the VM. Find who called into the VM host outside a running task. |

## Read first / read next

Read `moor/storage-and-state/transactions` before any of the children. Read the
child that matches your work. Read `moor/working-in-the-repo/testing` before you
add a test, because MOO-level behaviour is tested with `.moot` files rather than
Rust assertions.
