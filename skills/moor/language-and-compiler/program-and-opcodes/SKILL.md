---
name = "Compiled programs and the MOO opcode set"
brief = "What a compiled MOO Program holds, how names and jump labels work, how StoredProgram is persisted and versioned, and what invalidates a cached program."
when_to_use = "Use when adding or changing an opcode, reading or writing code generation or the decompiler, touching Program, PrgInner, Op, Name, Names, Label, Offset or StoredProgram, changing the moor_program FlatBuffer schema or the opcode word stream, or diagnosing a verb that will not load after an upgrade with a decode or builtin signature error. Use it before changing anything about the builtin function table, because a program records which builtins it calls. Not for what an opcode does when executed, which is moor/execution/virtual-machine. Not for MOO source syntax or the compiler front end, which is compiler-pipeline. Not for value types, which is value-model. Not for the Torchship database or in-world verb authoring. Not for Thetis internals."
universal = false
tags = ["moor", "moo", "opcode", "bytecode", "program", "storedprogram", "flatbuffers", "jump label", "names", "fork vector", "lambda", "program cache", "decode error", "builtin signature", "schema version"]
version = 1
---

# Compiled programs and the MOO opcode set

A `Program` is the output of the compiler and the input to the VM. It lives in
`crates/var/src/program/`, not in the compiler crate, because the database, the VM,
the RPC layer and the compiler all need it and none of them may depend on the
compiler.

## What a Program holds

`Program` is a thin handle over a reference-counted `PrgInner`. Cloning a `Program`
is a refcount bump, which is what lets the VM, the program cache and the database all
hold the same compiled verb.

`PrgInner` holds the instruction stream plus a set of side tables that instructions
index into:

| Field | Holds |
|---|---|
| `main_vector` | The opcodes of the verb body |
| `fork_vectors` | For each `fork` block: its offset in the main vector, and its own opcodes |
| `literals` | Every constant value the program mentions |
| `jump_labels` | The jump target table |
| `var_names` | The `Names` map: every variable declaration in the program |
| `scatter_tables` | Argument-destructuring specifications |
| `for_sequence_operands`, `for_range_operands` | Loop bindings and bounds |
| `range_comprehensions`, `list_comprehensions` | Comprehension bindings |
| `error_operands` | Error codes used by `MakeError` |
| `lambda_programs` | A complete nested `Program` for each lambda body |
| `main_max_stack`, `main_max_scope_depth` | Frame sizing, and the same per fork vector |
| `line_number_spans` | Opcode offset to line number, and the same per fork vector |

### Why the side tables exist

`Op` is fixed at sixteen bytes, and a unit test asserts it. Anything that does not
fit goes into a side table and the opcode carries an `Offset` into that table. This
is why `MakeError` takes an offset rather than an error code, and why a loop's
bindings live in `for_sequence_operands` rather than in the opcode.

Keep this rule when you add an opcode. If your variant would push `Op` past sixteen
bytes, add a side table. Growing `Op` costs memory across every program in a live
world and hurts instruction dispatch.

### A lambda is a whole nested program

`MakeLambda` names a scatter table for its parameters and an index into
`lambda_programs` for its body. The body is compiled at compile time into a
self-contained `Program` with its own names, literals and labels. `Capture` opcodes
immediately before it push the values that become the closure environment. The
decompiler reverses this by recursing into the nested program.

## Names, variables and scopes

Three types, easily confused:

- **`Variable`** is the compiler's identity for one declaration: a unique id, the
  scope id it was declared in, and either a `Symbol` or a register number.
- **`Name`** is the runtime address: an offset, a scope depth, and a scope id. This
  is what an opcode carries.
- **`Decl`** records how the variable was declared: the declaration type, the depth,
  whether it is constant, and the scope id. `Names.decls` maps `Name` to `Decl`.

`VarScope` in the compiler builds all of this while lowering, then `bind()` produces
the `Names` that goes into the program. `Names.global_width` is the size the frame's
environment must have when it is created; the VM uses it directly.

Two rules come from LambdaMOO and are visible here:

- **A set of global names always exists.** `GlobalName` lists them: `player`, `this`,
  `caller`, `verb`, `args`, `argstr`, `dobj`, `dobjstr`, `prepstr`, `iobj`,
  `iobjstr`. `VarScope::new` pre-declares every one, so they occupy the first slots
  of every program's environment. Get the current list from the enum.
- **An undeclared assignment declares a verb-global.** `find_or_add_name_global`
  implements it. `let` and `const` use the scoped path instead.

**Registers** are unnamed variables the compiler allocates for its own use, such as
the position counter of a list comprehension. They live in the same table and print
as a placeholder name. The decompiler must recognise them and not emit them as user
variables.

## Labels and offsets

- A **`Label`** is an index into `jump_labels`. A `JumpLabel` carries its id, an
  optional `Name` (for a labelled loop), and its resolved position.
- An **`Offset`** is an index into one of the side tables, or a fork vector index.

Code generation creates a label before it knows the target, then binds it when the
target is reached. This is why `Label` is an indirection rather than a direct
program counter: forward jumps are patched by binding the label, not by rewriting the
instruction.

The emitter's peephole fusion must not fold across a bound label. See
`compiler-pipeline`.

## The stored form

The database stores a `StoredProgram`: an opaque byte buffer holding a FlatBuffer.
The schema is `crates/schema/schema/moor_program.fbs`; the conversion is
`crates/schema/src/convert_program.rs` with `program_to_stored` and
`stored_to_program`.

The path is: disk bytes to `StoredProgram` to `Program` at load, and the reverse when
a verb is programmed. Decoding happens in `crates/db/src/provider/fjall_provider.rs`
when a verb is fetched.

A comment in `crates/var/src/program/stored_program.rs` says decoding happens in
`moor-compiler`. It does not; it happens in `moor-schema`.

### The opcode word stream

Opcodes are not stored as the Rust enum. `crates/schema/src/opcode_stream.rs` encodes
them into a stream of 16-bit words, with an explicit stable number for each opcode
declared as a constant.

**Those numbers are permanent.** The file says so and means it. Renaming an opcode is
free; renumbering one silently changes the meaning of every stored program in every
existing database. Add a new number at the end; never reuse a retired one.

The file's header says the encoding is designed so that the program counter is a
direct index into the word stream. That is a property of the encoding, not of the
running system. At run time the program counter indexes the decoded `Vec<Op>`, and
jump label positions are opcode indices in that vector. Do not assume word offsets
and program counters are the same thing.

### Versioning

`convert_program.rs` declares a current stored version and a minimum supported
version, as constants. Decoding rejects anything outside that range. Read the
constants for the current values; do not copy them into documentation.

The decoder already contains one compatibility branch: below the version that
introduced recorded stack depths, it synthesises conservative maximums from the
instruction count instead of reading them. That is the pattern to follow. Add a new
version constant, keep reading the old one, and derive what the old form did not
record.

The top-level stored program is a union over program languages with one variant
today. That union exists so a second language can be added without changing every
consumer.

### The builtin signature

Every stored program records a 64-bit signature over the builtins it calls. It is
computed by `builtin_signature_for_ids` in `crates/common/src/builtins.rs`: an
order-independent hash over each used builtin's id, name, override name, minimum and
maximum argument counts, argument types, and its implemented and exposed flags.

On decode the signature is recomputed from the decoded program and compared. A
mismatch is a hard decode failure with the message "Stored program builtin signature
mismatch".

This is deliberate. A program stores a builtin as a numeric id, so inserting a
builtin in the middle of the table would silently redirect every stored call past
that point. The signature turns silent corruption into a loud failure.

What it means for you:

- **Append new builtins at the end of their group.** A test in `builtins.rs` asserts
  one such adjacency; treat the whole table as append-only.
- **Changing an existing builtin's arity, argument types, name, override name,
  implemented flag or exposed flag invalidates every stored program that calls it.**
  Not just the ones you were thinking of.
- Read `moor/execution/builtin-functions` before touching that table.

## What invalidates a compiled program

Three different lifetimes, often confused:

| Level | What it is | Invalidated by |
|---|---|---|
| The stored bytes | The `StoredProgram` in the database | Programming the verb again. Nothing else rewrites it. |
| The decoded `Program` | Produced when the verb is fetched for execution | Nothing; it is immutable and refcounted. |
| The task program cache | `TaskProgramCache` in `crates/kernel/src/tasks/task_program_cache.rs` | Its own task ending. It is per task, not global. |

The cache is keyed by `VerbProgramKey`: the defining object and the verb's UUID. A
task that calls the same verb many times decodes it once. A verb that is reprogrammed
while a task is running keeps its old program for the rest of that task; the next
task gets the new one. That is the intended semantics, and it matches the fact that a
task is a transaction.

The cache hands the VM a raw pointer into a boxed slot and relies on that allocation
staying put. `reclaim_unreferenced` frees slots no live frame still points at. If you
change how frames hold programs, this is the code that breaks.

## Invariants

1. **`Op` is sixteen bytes.** Asserted by a unit test. Overflow goes to a side table.
2. **Stable opcode numbers never change and are never reused.**
3. **Every opcode has a decompiler arm.** An opcode the decompiler cannot reverse
   breaks `verb_code()` for every verb that contains it. See `compiler-pipeline`.
4. **Every opcode has a word-stream encode and decode arm.** A missing arm means the
   program cannot be stored.
5. **The stored version range is honoured on read and the current version is written
   on write.** Old versions are supported by deriving, not by rejecting.
6. **The builtin table is append-only, and existing entries keep their shape.**
7. **Jump label positions are opcode indices, not word offsets.**
8. **Line number spans refer to unparsed output lines.** See `compiler-pipeline`.
9. **A `Program` is immutable once built.** Everything shares it by reference count.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Verb fetch fails with "Failed to decode MooR program" | The stored bytes do not match the current decoder | Check the version constants first, then the word stream arms. |
| "Stored program version N is outside supported range" | The database was written by a newer build, or by one older than the minimum | Do not lower the minimum without adding the derivation branch for that version. |
| "Stored program builtin signature mismatch" | The builtin table changed shape since the verb was compiled | Revert the builtin change, or accept that every affected verb must be recompiled from source. |
| The VM panics on a stack underflow at a known program counter | Code generation emitted an unbalanced sequence | The bug is in `backend/`, not in the VM. |
| A newly added opcode round-trips in tests but breaks an existing database | The opcode number collided with a retired one | Numbers are permanent. Pick the next unused one. |

### The recovery path, and its trap

There is no bulk re-encode of stored programs. The database migration in
`crates/db/src/provider/fjall_migration.rs` updates a version marker only. So the
recovery from a stored-program format break is: export the world to objdef source and
import it into a fresh database.

**That export reads the same stored programs, decodes them, and decompiles them.** If
they no longer decode, you cannot export them either. Export *before* you land a
change that could break the format, not after. See
`moor/content-pipeline/objdef-format`.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/var/src/program/program.rs` | `Program`, `PrgInner`, the side-table accessors |
| `crates/var/src/program/opcode.rs` | `Op`, the operand structs, and the size assertion |
| `crates/var/src/program/names.rs` | `Name`, `Variable`, `Names`, `GlobalName` |
| `crates/var/src/program/mod.rs` | `ProgramType`, `Decl`, `DeclType` |
| `crates/var/src/program/labels.rs` | `Label`, `JumpLabel`, `Offset` |
| `crates/var/src/program/stored_program.rs` | The opaque stored byte wrapper |
| `crates/schema/schema/moor_program.fbs` | The stored FlatBuffer schema |
| `crates/schema/src/convert_program.rs` | Encode, decode, version range, signature check |
| `crates/schema/src/opcode_stream.rs` | The stable opcode numbers and the word stream |
| `crates/common/src/builtins.rs` | The builtin table and `builtin_signature_for_ids` |
| `crates/kernel/src/tasks/task_program_cache.rs` | The per-task decoded program cache |
| `crates/db/src/provider/fjall_provider.rs` | Where a stored program is decoded on fetch |

## Read first / read next

Read `value-model` first: literals in a program are `Var`s, and the reference-counting
rules there explain why sharing a `Program` is cheap.

After this, read `moor/execution/virtual-machine` for how a frame is built from
`main_max_stack`, `main_max_scope_depth` and `global_width`, and
`moor/services/wire-schema` for the wider rules on changing a FlatBuffer schema that
data on disk already uses.
