---
name = "Objdef: a world as a directory of source files"
brief = "How mooR's objdef directories work: the file layout, object identity and constants, the multi-pass import, what round-trips, and how to load or replace one object safely."
when_to_use = "Use when you read, write, import, export or diff objdef .moo files: constants.moo, import_export_id, import_export_hierarchy, define declarations, include!/include_bin!, property overrides, verb and method blocks, checkpoint exports, or the moor-objdef crate. Use it also for load_object, reload_object, dump_object and parse_objdef_constants, for moor-emh load and reload, for conflict modes clobber/skip/detect and entity overrides, and when an import fails to compile a verb or reports a duplicate object. Not for the LambdaMOO textdump format (read textdump-compat), not for choosing or starting a core (read cores-and-bootstrap), not for the MOO language or the compiler itself, not for the Torchship game database, not for in-world verb authoring for a specific game, and not for Thetis's own internals."
universal = false
tags = ["moor", "objdef", "moo files", "constants.moo", "import_export_id", "load_object", "reload_object", "dump_object", "checkpoint", "export", "conflict", "clobber", "skip", "moor-objdef", "round trip", "version control"]
related = ["moor/language-and-compiler/compiler-pipeline", "moor/storage-and-state/world-state-model"]
version = 1
---

# Objdef: a world as a directory of source files

This skill is written in ASD-STE100 Simplified Technical English.

Objdef is mooR's own database format. One object is one text file. A world is a
directory tree of those files plus a `constants.moo` index. The `moor-objdef` crate
in `crates/objdef` parses, applies, collects and writes them. The grammar lives with
the compiler, in `crates/compiler/src/objdef.rs` and `objdef_literal.rs`.

## Why the format exists

LambdaMOO kept a world in one dump file that only the server wrote. You could not
review a change to it, merge two changes to it, or see which object a diff touched.
Objdef exists to make a world an ordinary source tree.

The consequences are the point of the design:

1. **A world is reviewable.** One object per file means a diff names the object.
2. **A world is mergeable.** Two authors who touch different objects touch different
   files.
3. **A world has a canonical form.** An export is deterministic enough to commit:
   properties are sorted by name, and object references are written as symbolic
   constants instead of numbers where a name exists.
4. **A world can be built from source by a tool, not only by a server.** `moorc`
   compiles a directory into a database without starting a daemon.

The cost is that objdef is not the live world. The database is. The two diverge as
soon as anybody programs from inside the running server.

## Directory layout

| Path | Content |
|---|---|
| `constants.moo` | `define NAME = #n;` for every named object. Read first, from the root only. |
| `<name>.moo` | One object, named from its `import_export_id`. |
| `object_<n>.moo`, `player_<n>.moo` | One object with no `import_export_id`. `player_` when the object has the player flag. |
| `<dir>/...` | A subtree named by the object's `import_export_hierarchy`. |
| `_anonymous_objects.moo` | Every anonymous object of one hierarchy group, in one file. |

The importer collects `.moo` files recursively. Only a `constants.moo` in the root
directory is treated as the constants file; a `constants.moo` deeper in the tree is
parsed as an ordinary source file.

## Object identity

An object's number is its identity in the database. Its *name* in the source tree is
separate, and comes from object metadata:

- `import_export_id` gives the constant name (upper case) and the file name (lower
  case).
- `import_export_hierarchy` gives the subdirectory path, as a string or a list.

Both key names are defined in `crates/objdef/src/lib.rs`. Both may also appear as
ordinary properties on old objects; the dumper promotes such a property into metadata
and does not write it out as a property.

The dumper refuses to name an object when its `import_export_id` is not unique, or
when it equals the parent's value and was therefore inherited rather than set. Such
an object falls back to a numbered file name and gets no constant. A duplicate is
logged as a warning, not an error, so a core can silently lose a stable file name.

Where do the identifiers come from on a first import?

| Source | How identity is established |
|---|---|
| Objdef directory | After the load, if **no** object in the set carries an `import_export_id`, one is created for every object named by a constant, from the lower-cased constant name. If any object has one, nothing is inferred. |
| Textdump | From the properties on `#0` that hold object values. See `textdump-compat`. |

## Constants

`define NAME = <literal>;` declares a constant. Constants are shared across every
file of one import, because the whole set is parsed through one `ObjFileContext`.

Two rules surprise people, and both raise an error at parse time:

1. A constant name may not be declared twice.
2. **Two constants may not have the same value.** `define A = #1; define B = #1;` is
   rejected as a duplicate. The check compares values, not names.

`include!("path")` and `include_bin!("path")` splice a file into a literal position as
a string or as binary. Both resolve relative to the file being parsed, and both are
refused if the resolved path leaves the import root directory.

## What round-trips, and what does not

The engine stores a verb compiled. An export decompiles the stored program and
unparses it. Therefore an export is not the text you wrote.

| Survives export and re-import | Does not survive |
|---|---|
| Object number, name, parent, location, owner, flags | Comments outside verb bodies, and comments inside them |
| Property definitions, values, owner and flags | Original whitespace, indentation and line breaks in verb code |
| Property overrides that differ from the definer | An override whose value equals the inherited value; it is written as inheriting |
| Verb names, argument specification, owner, flags, program | The distinction between "explicitly set to the inherited value" and "clear" |
| Object, property and verb metadata maps | Verb order relative to a hand-edited file, if it was edited out of order |
| Anonymous objects, as one grouped file | Suspended and forked tasks. Objdef holds world state only. |

Two mitigations matter. A comment written as a MOO string statement inside a verb is
part of the compiled program, so it survives. Property definitions are sorted by name
on export, and verbs keep their stored order, so repeated exports of an unchanged
world are stable.

## The import path

Import is multi-pass because an objdef may name any object in the set, including one
whose file has not been read yet, and because a verb program may reference a parent
that does not exist yet.

| Pass | What it does | Why it cannot be later |
|---|---|---|
| 1. Parse | Read `constants.moo`, then every `.moo` file, through one context. Reject duplicate object definitions. | Constants must exist before any file that uses them. |
| 2. Create placeholders | Create every object in the set with no parent, no location and no owner. | Every reference in later passes must resolve to a real object. |
| 3. Attributes | Set parent, location and owner. Set object flags. | Property definition and verb definition depend on ancestry. |
| 4. Object metadata | Apply the object metadata map. | Identity data must exist before a later dump. |
| 5. Property definitions | Define each locally defined property. | Overrides need the definer to exist. |
| 6. Property overrides | Apply overriding values and permissions. | — |
| 7. Verbs | Compile and add or update each verb. | Compilation may call anything; it runs last so the world is complete. |

Consequences to remember:

- **Verb compilation is the last thing that happens.** An import that fails on a verb
  has already applied every object, property and attribute in the same transaction.
  The transaction is dropped, so nothing lands, but the error you see is late.
- **Unknown builtin functions are allowed during a directory import.** The loader
  turns on `call_unsupported_builtins`, so a call to a function the server does not
  have compiles into a runtime call and fails only when the verb runs.
- **A bulk import does not validate parent references.** Parent validation is off for
  directory and textdump imports. A parent that names an object outside the set and
  outside the database is written as given. There is no error at import time.

## Conflicts, and updating one object

The same loader serves whole-directory import and single-object load, so it always
compares incoming state against existing state. A difference is a *conflict*.

| Conflict mode | Behaviour |
|---|---|
| `Clobber` | Overwrite the existing entity. The default. |
| `Skip` | Keep the existing entity, and only add entities that do not conflict. |
| `detect` (tools and builtins only) | Dry run plus conflict reporting. |

Conflicts are recorded per entity, so a caller can override one of them without
changing the mode for everything. The entity classes are object flags, builtin
properties (location and owner), parentage, a property definition, a property value,
a property flag, a verb definition and a verb program. The current list is the
`Entity` enum in `crates/objdef/src/load.rs`; the MOO-visible spelling of each is in
`crates/kernel/src/vm/builtins/bf_obj_load.rs`.

Three entry points apply one object:

| Entry point | Semantics |
|---|---|
| `load_object()` builtin, and `load` in `moor-emh` | Add or merge one object definition. Honours conflict mode, overrides and a target object kind. Validates parent changes. |
| `reload_object()` builtin, and `reload` in `moor-emh` | Replace one object. Deletes locally defined properties and verbs that the incoming definition does not name, then applies everything in clobber mode. |
| `dump_object()` builtin, and `dump` in `moor-emh` | Produce the objdef text for one object, as a list of lines. |

Rules for doing this safely on a live world:

1. **Verbs are matched by any shared name.** A verb is found by name overlap, not by
   the exact name list. Add an alias and you update the existing verb; remove the
   only shared alias and you create a second verb instead of renaming the first.
2. **`load_object` does not rename an existing object.** When the object already
   exists the loader keeps it and applies attributes, but the object's name is not
   among the attributes it applies. `reload_object` does set the name.
3. **`reload_object` deletes.** It removes verbs and locally defined properties that
   are absent from the incoming text. Descendants that relied on those properties
   lose them. Dump the object first.
4. **A dry run is not a rollback.** `dry_run` only sets the `commit` flag in the
   result. The loader has already mutated the transaction it was given. The daemon's
   import path drops that transaction, so a dry run there is safe. Inside a running
   server the mutations are in the calling task's transaction, and they commit with
   the task unless the task aborts. Raise an error after a dry run, or do it in a
   throwaway world. The book's `object-loading.md` says a dry run makes no changes;
   that is true only of the import path.
5. **These builtins bypass permission checks and are wizard-only.** They run against
   the loader interface, through `with_loader_interface` in
   `crates/kernel/src/task_context.rs`, on the calling task's own transaction.

## Invariants

1. Every object in an import set exists before any attribute, property or verb is
   applied. Break this and references inside the set stop resolving.
2. One import is one transaction, and it commits once or not at all.
3. Two constants never share a value, and one object ID is never defined twice in one
   set. Both are parse-time errors, not load-time errors.
4. An `include!` path never leaves the import root directory.
5. An export is produced from a read-only snapshot, never from a live transaction.
6. An `import_export_id` is unique across the world. When it is not, the dumper drops
   the name and writes a numbered file instead.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/compiler/src/objdef.rs` | The object definition types, the parse context, and the parse error set. |
| `crates/compiler/src/objdef_literal.rs` | The whole objdef grammar: objects, properties, verbs, literals, `define`, `include!`. |
| `crates/objdef/src/set.rs` | Parse a set of sources into a proposed object graph. No database effects. |
| `crates/objdef/src/load.rs` | Apply a graph: placeholders, phases, conflict handling, single-object load and reload. |
| `crates/objdef/src/dump.rs` | Collect definitions from a snapshot, choose names and hierarchy, write the directory. |
| `crates/objdef/src/write.rs` | Serialise one object, and generate `constants.moo`. |
| `crates/objdef/src/conflict_tests.rs` | The conflict behaviour, as executable specification. |
| `crates/kernel/src/vm/builtins/bf_obj_load.rs` | `dump_object`, `load_object`, `reload_object`, `parse_objdef_constants`. |
| `crates/kernel/src/tasks/checkpoint.rs` | The checkpoint export: snapshot, collect, dump, rename. |
| `tools/moor-emh/src/commands.rs` | The offline `dump`, `export`, `load` and `reload` commands. |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Import stops with a verb compile error naming a file | A verb body does not compile under the current feature flags | Compare the flags the core needs with the flags the server runs. Read `moor/language-and-compiler/language-features-and-compat`. |
| `Duplicate object definition for #n` | Two files define the same object number | One of them is a stale copy. The error names both sources. |
| `Duplicate constant` on a constant you did not repeat | Another constant already has that value | Constants are unique by value as well as by name. |
| `Include error ... path escapes the source directory` | An `include!` reached outside the import root | Move the included file inside the tree. |
| `Cannot dump object #n: verb <i> has an empty name` | A verb in the database has no usable name | Repair it in-world with `set_verb_info` before the next checkpoint. The whole export fails until then. |
| An export writes numbered files where names used to be | Duplicate or inherited `import_export_id` | Look for the duplicate warning in the log and give one object a distinct value. |
| An import "succeeds" but an object has an invalid parent | Bulk import does not validate parents | Validate deliberately: load the object again through `load_object`, which does validate. |
| A dry run changed the world | The task committed | See rule 4 above. |
| A checkpoint directory is named `.in-progress` | The export failed part way | The rename to `.moo` is the last step. Read the log for the dump error. |

## Read first, read next

Read first:

- `moor/storage-and-state/world-state-model` — what a property definition, an
  override and a verb resolution actually are.
- `moor/language-and-compiler/compiler-pipeline` — why an exported verb body is
  decompiled text and not your source.

Read next:

- `textdump-compat` — the other import format, and the identity bridge from `#0`.
- `cores-and-bootstrap` — how a directory becomes a running world.
- `moor/working-in-the-repo/repo-tooling` — `moorc` and `moor-emh`.
- `book/src/the-system/objdef-file-format.md` — the syntax reference. Good, but check
  the code for anything load-bearing.
