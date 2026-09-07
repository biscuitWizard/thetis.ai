---
name = "Object lifecycle and garbage collection"
brief = "How mooR creates, recycles, renumbers and collects objects, what makes an anonymous object unreachable, and why object references are unsafe to hold."
when_to_use = "Use when work touches object creation, recycle, renumber, anonymous objects, or the mark-and-sweep collector, or an object that vanished or should have vanished. Not for transaction conflict, the relation model, or the on-disk format, which the sibling skills own, and not for the Torchship database."
universal = false
tags = ["moor", "objects", "create", "recycle", "renumber", "object number allocation", "anonymous objects", "garbage collection", "gc", "gc_interval", "mark and sweep", "gc mark thread", "sweep pause", "object numbers", "uuid objects", "reachability", "gcinterface", "moor-db"]
version = 2
---

# Object lifecycle and garbage collection

An object exists when it has a row in the `object_flags` relation and it stops
existing when that row is removed. Everything else on this page follows from that,
and from one design choice: most objects are destroyed by an explicit call, and one
kind is destroyed by a collector.

## The three kinds, and who destroys them

| Kind | Identity | Destroyed by |
|---|---|---|
| Numbered | A signed 32-bit number from a shared sequence | An explicit `recycle` |
| UUID | A generated identifier, not sequential | An explicit `recycle` |
| Anonymous | A generated identifier with no printable literal form | The garbage collector only |

Anonymous objects are gated by an `anonymous_objects` server feature flag, which is
off unless it is turned on. Turning it on turns on the collector's cost.

The MOO `recycle` builtin refuses an anonymous object outright.

## Create

`create_object` in the engine takes an object kind and a set of attributes. In order,
it writes the owner, the name, the parent, the location, and the flags. Every one of
those writes is marked as guaranteed unique, because the identifier is new and cannot
collide, and so it skips the commit-time conflict check.

Two points decide correctness:

- **The owner defaults to the object itself** when no owner is given.
- **The maximum-object sequence is raised** to the new number, but only for numbered
  objects. UUID and anonymous objects do not move it. That is what lets an import
  place objects at arbitrary numbers and still leave the sequence correct.

The database does not call MOO verbs. The `initialize` verb is called by the builtin
that wraps creation, not by the storage layer. Keep it that way: a database operation
that could call arbitrary MOO code would re-enter the transaction.

## Object numbers are never reused

The sequence counter lives on the shared database handle, not in a transaction. It is
incremented at allocation and it is never rolled back.

Therefore:

- A transaction that conflicts and retries has already consumed a number. The number
  is skipped.
- A recycled object's number is not handed out again.
- Gaps in the numbering are normal and are not a fault. Do not add code to reclaim
  them.

The reason is safety: a stale reference to a recycled object must never silently
become a reference to a different, new object. UUID objects avoid the exhaustion
question entirely.

## Recycle

`recycle_object` performs a fixed sequence. The order matters, because the contents
and children lists are derived from the forward relations and would otherwise be read
after they were destroyed.

1. Read contents, parent and children **first**, before any mutation.
2. Move every contained object to nothing.
3. Reparent every child to this object's parent.
4. Delete the parent and location tuples, which also removes the derived reverse
   entries.
5. Delete flags, name, owner, verb definitions and all metadata.
6. Delete every property value row for the object's own property definitions, then
   the property definition set.
7. Invalidate the verb, property and ancestry caches for the object.

The caller is responsible for the MOO-visible part: calling `recycle` on the object
and `exitfunc` on each thing it contained. The builtin does that with trampolines
before it reaches the database. `recycle_objects` is the batched form used by the
collector's sweep; it does the same work with fewer cache flushes.

Permission to recycle is checked in `DbWorldState`, through an object-recycle rule.
`check_recycle_object` exists so a caller can test the permission before it starts the
verb calls that cannot be undone.

## Renumber

`renumber_object` moves an object to a new identifier and updates the structural
relations. It follows LambdaMOO semantics: it does **not** rewrite references to the
object held in property values or in verb code. Anything that stored the old
identifier now points at nothing. It also invalidates the caches for the old object
and for the whole new branch.

## What makes an anonymous object unreachable

The collector is mark and sweep, and it is only for anonymous objects.

**Roots** are two sets:

- References found in the virtual machine state of **suspended** tasks, both the live
  state and the retry state.
- References held by any non-anonymous object in the database.

**Database references** are found by scanning, not by a reference count:

- Every property value, scanned as a whole value. Lists, maps, flyweight delegates
  and slots, error values and lambda captured environments are all walked.
- Every metadata value.
- Parent, location and contents relationships.
- The location and owner fields of verb definitions.
- The definer and location fields of property definitions.

Property values are scanned across the whole relation rather than per object, because
a child that overrides an inherited property stores the value under its own key while
the definition lives on an ancestor.

From those roots the mark phase takes a transitive closure over anonymous-to-anonymous
references. Anything not reached is unreachable.

## The collection cycle

| Phase | Where | Blocking |
|---|---|---|
| Trigger | The scheduler, on a `gc_interval`, or forced by a MOO builtin | No |
| Mark | A separate thread, on its own read transaction | No. Tasks keep running. |
| Sweep | The scheduler | Yes. New tasks are blocked and the scheduler waits for every active task to finish. |

The sweep is guarded twice. It records the last mutation timestamp before it waits
for active tasks, and compares it after. If any mutation happened while it waited,
the mark result may be stale and the sweep is abandoned for this cycle. The sweep
then recycles the still-valid unreachable objects in one batch transaction. If that
transaction conflicts, the cycle is retried a small number of times and then given
up.

Note what this implies: **only suspended tasks contribute virtual-machine roots.**
The design is safe because the sweep waits for every active task to end and abandons
itself if the world changed. It would not be safe if either guard were removed.

## Hazards of holding an object reference across a transaction

This is the part that bites verb authors and server code alike.

| Hazard | Why | What to do |
|---|---|---|
| An object reference is a bare identifier, not a handle | Nothing keeps the object alive because you hold the value | Test with `valid` after every transaction boundary |
| An object may be recycled between your transactions | Another task committed a recycle | Re-read what you need; do not cache attributes across a suspend |
| A renumbered object leaves your stored reference pointing at nothing | Renumber does not rewrite stored references | Store a system reference or a property, not a raw number |
| An anonymous object held only in a **running** task's registers is not a mark root | Only suspended tasks are scanned | Do not rely on this. Store it somewhere reachable if it must survive. |
| An anonymous object stored only in a variable across a suspend survives | The suspended task's state is scanned | This is intended, and it is why suspended state is a root |
| A retry re-runs your creation | Creation is not rolled back for sequence purposes | Expect skipped numbers; do not assume the object you created in the first attempt exists |

## Invariants

1. **Existence is a row in `object_flags`.** Any create must write it and any recycle
   must remove it. A partial recycle leaves an object that is valid but has no name,
   parent or owner.
2. **Read the derived lists before you mutate the forward relations.** Contents and
   children come from reverse lookups. Reading them after the delete gives the wrong
   answer.
3. **Sequence counters move forward only, and are never rolled back.** Object numbers
   are not reusable, by design.
4. **The database calls no MOO verb.** Lifecycle verbs are the caller's job.
5. **A new reference-bearing place must be added to the reference scan.** If you add
   a relation, or a value shape that can hold an object, and it is not scanned, live
   anonymous objects will be collected.
6. **The sweep must run with no active tasks and with no mutation since the mark.**
   Both guards exist for the same reason. Removing either allows a live object to be
   collected.
7. **Recycling must invalidate the caches for the object.** Otherwise a verb or
   property still resolves on an object that no longer exists.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| An anonymous object disappeared while still in use | A reference lives somewhere the scan does not look | Add that place to the reference scan. Check value shapes first: flyweights, lambdas, error values. |
| Anonymous objects accumulate and are never collected | The collector never runs, or the mark is always invalidated | Check the GC interval setting. Under constant mutation the sweep abandons itself every cycle. |
| A visible pause on a busy server | The sweep waits for every active task | Expected. It is bounded by the slowest running task. |
| "GC transaction conflict" in the log | The sweep's own transaction lost | It retries a few times. Repeated failure means the world mutates faster than the sweep can commit. |
| `E_INVARG` from `recycle` | The argument was an anonymous object, or not valid | Anonymous objects are collector-only. |
| Object numbers jump | Retries and failed transactions consumed sequence values | Expected. |
| Code breaks after a renumber | Stored references were not rewritten | Expected LambdaMOO behaviour. Use symbolic references. |
| An object is valid but has no parent, name or owner | An interrupted or partial recycle | Treat as a data fault; find the path that deleted some rows and not others. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/db/src/engine/ws_transaction.rs` | `create_object`, `recycle_object`, `recycle_objects`, `renumber_object`, the reference scan, and the cache invalidation helpers |
| `crates/db/src/api/gc.rs` | The `GCInterface` trait and its errors |
| `crates/db/src/api/world_state.rs` | The permission-checked create, recycle and renumber |
| `crates/db/src/model/mod.rs` | Extraction of object references from a value, including nested shapes |
| `crates/kernel/src/tasks/gc_thread.rs` | The mark phase and the transitive closure |
| `crates/kernel/src/tasks/scheduler/scheduler_gc.rs` | Triggering, the sweep pause, the mutation-timestamp guards, and the retry |
| `crates/kernel/src/tasks/task_q.rs` | Collecting virtual-machine roots from suspended tasks |
| `crates/kernel/src/vm/builtins/bf_objects.rs` | `create`, `recycle`, `renumber` and the lifecycle verb calls |
| `crates/db/src/api/gc_tests.rs` | Reachability tests for the collector |
| `crates/testing/load-tools/src/anonymous-object-load-test.rs` | Load exercise for anonymous objects |

## Read first, read next

Read `world-state-model` first; recycle is a sequence of relation deletes and it is
not understandable without the relation table. Read `transactions` for why a failed
create still consumes a number. Read `moor/execution/task-scheduler` for what a
suspended task is and why its state is a root. Read
`moor/language-and-compiler/value-model` for the value shapes that can hold an object
reference, which is exactly the list the reference scan must cover.
