---
name = "The mooR bytecode VM"
brief = "How the MOO interpreter runs a verb: activations and frames, verb call setup, tick slices, error raising and stack unwinding."
when_to_use = "Use when working on MOO code execution inside the mooR server: the activation stack, how a verb or lambda call is set up, opcode dispatch and tick counting, try/catch/finally unwinding, or a stack-depth or execution-result question. Not for the compiler or opcode set itself, writing a builtin, scheduler queues, or permission rules, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "vm", "interpreter", "activation", "frame", "execstate", "moostackframe", "value stack", "scope stack", "unwind", "traceback", "e_maxrec", "verb d flag", "opcode", "tick", "try catch finally", "program cache", "executionresult"]
version = 2
---

# The mooR bytecode VM

The VM executes one MOO program at a time inside one task. It is a stack machine
over the opcode vector the compiler produced. It has no state of its own: all of
it lives in the task's `ExecState`, which is snapshotted for retry and serialised
for suspension.

Read `moor/execution` and `moor/language-and-compiler/program-and-opcodes` first.

## The split between two crates

`crates/vm` is the interpreter proper. It knows nothing about the database, the
scheduler, or the builtin table. Everything it needs from outside is behind the
`VmHost` trait: read a property, write a property, read object flags, check
validity, dispatch a verb, get a parent, retrieve a verb program.

`crates/kernel/src/vm` is the host side. `KernelHost` implements `VmHost` against
the running task's transaction. `VmHost` (the struct, in `vm_host.rs`, a different
thing from the trait) drives the interpreter, applies the tick and time limits,
resolves programs through the task program cache, and turns interpreter results
into the responses the task loop understands.

Keep that split. A change in `crates/vm` that needs the database means the
abstraction is in the wrong place; add a trait method instead.

## Activations and frames

`ExecState` holds a vector of `Activation`. The last element is the running one.
An activation carries the things that are the same for the whole call: `this`,
the player, the verb name, the resolved verb definition, and the task permissions
under which it runs.

Inside the activation is a `Frame`, of one of two kinds.

| Frame | Holds |
|---|---|
| `Moo` | A `MooStackFrame`: the program, the program counter, the variable environment, the value stack, the scope stack, and scratch space. |
| `Bf` | A `BuiltinFrame`: the builtin id, its arguments, an optional trampoline number and argument, and a return value slot. |

Builtin frames are transient. Anything that asks "who am I running as" walks past
them to the nearest MOO frame. That is why `caller_perms()`, `task_perms()`, and
`this()` all skip `Bf` frames.

A `MooStackFrame` keeps scopes in a separate stack from values. Entering a scope
records the value stack height; leaving it truncates back. That is what makes
`try`, `finally`, loops, and lexical blocks unwind cleanly without walking values.

## Setting up a verb call

Two things happen, in this order, and they use different principals.

1. **Resolution.** `dispatch_verb` on the world state resolves the name up the
   inheritance chain, using the *caller's* permissions. Method lookup requires the
   `x` flag, so a verb without `x` is not found at all.
2. **Activation.** The new activation runs as the *resolved verb's owner*, with
   the flags dispatch selected. `Activation::for_call` builds the frame and sets
   the authority.

The program itself is not looked up by the interpreter. The interpreter yields
`DispatchVerb` with a `VerbExecutionRequest`, and the VM host resolves the program
through the task program cache. The cache is task-local, keyed by verb definer and
verb uuid, and valid only for the life of the task's transaction. Frames may hold
a raw pointer into it, which is why a frame handed to another task, or to
persistence, must materialise its program first.

Command dispatch is the same shape with `DispatchCommandVerb`, which additionally
seeds `dobj`, `iobj`, `prepstr`, `argstr`, and the rest.

Lambdas are the third shape: a lambda activation inherits the caller's context and
rebuilds the captured environment into the new frame's scopes.

## The execution loop

The task loop calls into the VM host, which calls `moo_frame_execute` for a MOO
frame or re-enters the builtin for a `Bf` frame. `moo_frame_execute` runs opcodes
until one of three things happens: the tick slice is spent, the program ends, or
an opcode yields a result the frame cannot handle alone.

Tick accounting has three separate numbers. Do not confuse them.

| Number | Meaning |
|---|---|
| `tick_count` | Total opcodes this task has executed since it last resumed. |
| `tick_slice` | The most this one call into the interpreter may execute, so the loop stays responsive. |
| `max_ticks` | The task's whole budget, from `fg_ticks` or `bg_ticks`. Checked by the VM host, not inside the opcode loop. |

Elapsed time is checked every sixty-fourth tick, not every tick.

The VM host then interprets the interpreter's `ExecutionResult`:

| Result | Host action |
|---|---|
| `More` | Return to the task loop and come back. |
| `PushError`, `RaiseError` | Apply the `d` flag rule below. |
| `Return`, `Unwind` | Unwind the stack. |
| `DispatchVerb`, `DispatchCommandVerb`, `DispatchEval`, `DispatchLambda` | Push a new activation. |
| `DispatchBuiltin` | Call the builtin. |
| `TaskStartFork`, `TaskSuspend`, `TaskNeedInput` | Yield to the scheduler. |
| `Complete`, `Exception` | Finish the task. |
| `TaskRollbackRestart`, `TaskRollback` | Roll back; retry or die. |

Stack depth is checked before each entry into the interpreter. Exceeding
`max_stack_depth` raises `E_MAXREC` unconditionally, ignoring the `d` flag,
because a non-raising recursion overflow would be silent and unbounded.

## Builtins that must call back into MOO

A builtin runs to completion inside one Rust call. Some builtins nonetheless need
to run MOO code in the middle: `create()` calls `:initialize`, `recycle()` calls
`:exitfunc`, `move()` calls `:accept` and the enter and exit functions, and
`eval()` runs a compiled program.

The mechanism is a **trampoline**. The builtin writes a small integer and an
optional value into its own `BuiltinFrame`, then returns a `VmInstr` asking the VM
to dispatch a verb. The builtin's frame stays on the stack underneath. When the
called verb returns, the VM re-enters the same builtin, which reads the trampoline
number back and continues from that point.

Consequences a builtin author must respect:

- A builtin with no trampoline set is assumed finished when re-entered, and its
  return value is unwound immediately.
- The trampoline number is private to that builtin. An unknown value is a panic,
  by design; it means a code path forgot to set one.
- Trampoline state lives in the frame, so it survives a suspension inside the
  nested verb.

## Error propagation

The `d` flag on the verb definition decides whether an error becomes an exception.

| Situation | With `d` set | Without `d` |
|---|---|---|
| An operation produces an error value | The error is raised and unwinds | The error value is pushed as the expression's result and execution continues |
| `raise()` in MOO code | Raises | Does not raise |
| Stack depth exceeded | Raises | Raises anyway |

`push_error` and `raise_error` in `ExecState` both consult the nearest non-builtin
activation for the flag. A builtin frame inherits the calling verb's flags for
exactly this purpose.

Unwinding walks activations from the top:

1. An `Exit` reason jumps within the frame and stops.
2. A `TryFinally` scope jumps to its finally label, pushes the reason, and stops.
   The reason is re-thrown when the finally block ends.
3. A `TryCatch` scope whose catch list matches the raised error jumps to the catch
   label, pushes the catch value, and stops.
4. Otherwise the scope is discarded and the walk continues. When the frame runs
   out of scopes, the activation pops.
5. A `Return` reason that pops an activation sets the return value in the caller
   and stops there.
6. If the stack empties, the reason becomes the task's outcome: a `Return` becomes
   success, a fallthrough becomes a false value, anything else becomes an exception.

A raised exception carries a stack list and, at the very top, a rendered backtrace.
The stack list starts at the first activation that could have handled the error,
so a traceback does not leak frames above a wizard-owned boundary that already
declined it.

## Why it is a plain interpreter

There is no JIT, no threaded dispatch beyond a match on the opcode, and no
inlining of MOO into MOO. The reasons are structural, not laziness:

- The task's tick budget must be exact and cheap to check. A tick is one loop
  iteration. A cleverer execution strategy would make the count approximate, and
  the count is user-visible through `ticks_left()`.
- Verb programs change at runtime. A programmer can reprogram a verb from inside
  the world, and the next call must see it. The task program cache exists to make
  repeat lookups cheap within a transaction, and is discarded when that
  transaction ends.
- Every property and verb access must pass a permission check against the current
  transaction. There is nothing to hoist out of the loop.
- The whole VM state must be snapshotted for conflict retry and serialised for
  suspension. Anything that hid state in native frames or in generated code would
  break both.

The optimisations that are present are narrow: a cached opcode pointer for the
duration of one execute call, cold-path helpers for error construction, a
task-local program cache, and the tick slice that bounds one call. Keep new work
inside that shape.

## Invariants

1. **The activation stack is never empty while a task runs.** Kernel code that
   reads `this`, the verb name, or the line number from an empty stack panics on
   purpose. Fix the caller.
2. **A builtin frame is never the answer to a permissions or identity question.**
   Skip `Bf` frames when you walk the stack for a principal.
3. **`pc_type` does not change while a frame executes.** The opcode vector is
   selected once per execute call, and a change mid-call is undefined.
4. **A cached program pointer is valid only inside the owning task's
   transaction.** Materialise the program before a frame leaves the task, as fork
   handoff does.
5. **`crates/vm` has no database dependency.** Add to the `VmHost` trait rather
   than reaching around it.
6. **A builtin returns a value, an error, or a VM instruction — never nothing.**
   Returning the none type is a debug assertion failure.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Panic: "activation stack underflow" | Something read the top activation with an empty stack | Find the caller. This is never a VM bug on its own. |
| Panic: "Expected a BF frame at the top of the stack" | Builtin helper used outside a builtin call | The helper is being reused from ordinary kernel code. |
| Panic: "Invalid trampoline for bf_..." | A builtin resumed with a trampoline number it does not handle | Add the arm, or stop setting that number. |
| `E_MAXREC` where recursion looks shallow | `max_stack_depth` was lowered on `$server_options`, or a builtin trampoline is looping | Check the depth setting first, then the trampoline. |
| An error silently becomes a value in the middle of a verb | The verb has no `d` flag | Expected LambdaMOO behaviour. Set `d` on the verb if you want raising. |
| Panic: "PC out of range for opcode stream" (debug builds) | The compiler emitted a jump past the end, or an opcode advanced the PC wrongly | A compiler or opcode bug. See `moor/language-and-compiler/program-and-opcodes`. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/vm/src/lib.rs` | The `VmHost` trait: everything the interpreter needs from outside. |
| `crates/vm/src/exec_state.rs` | The activation stack, call setup, principal lookup, `set_task_perms`, error entry points. |
| `crates/vm/src/activation.rs` | `Activation`, `BuiltinFrame`, `Frame`, and how a call's authority is chosen. |
| `crates/vm/src/moo_frame.rs` | `MooStackFrame`, scopes, the environment, the program slot. |
| `crates/vm/src/moo_execute.rs` | The opcode loop, `ExecutionResult`, `TaskSuspend`, `Fork`. |
| `crates/vm/src/vm_unwind.rs` | `FinallyReason`, unwinding, stack lists and backtraces. |
| `crates/vm/src/environment.rs` | Variable scopes and their widths. |
| `crates/vm/src/scatter_assign.rs` | Scatter assignment semantics. |
| `crates/kernel/src/vm/vm_host.rs` | Limits, program cache resolution, translating interpreter results. |
| `crates/kernel/src/vm/kernel_host.rs` | `VmHost` implemented against the task's transaction. |
| `crates/kernel/src/vm/vm_call.rs` | Calling and re-entering builtins, and the `bf_*` proxy check. |
| `crates/kernel/src/tasks/task_program_cache.rs` | The task-local verb program cache. |

## Read first / read next

Read `moor/language-and-compiler/program-and-opcodes` for what the opcode vector
contains, and `moor/language-and-compiler/value-model` for `Var` and `Error`. Read
`builtin-functions` before you touch anything under `crates/kernel/src/vm/builtins`.
Read `task-scheduler` for what happens after the VM yields.
