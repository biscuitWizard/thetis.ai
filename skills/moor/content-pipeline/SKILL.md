---
name = "Getting a world into and out of mooR"
brief = "Choose the right part of mooR's import and export path: objdef source directories, LambdaMOO textdump import, and the cores bundled in the repository."
when_to_use = "Use when a mooR database must be created, exported, updated or moved, or when a world starts but nobody can log in. Use it to pick the child skill. Not for the MOO language or the compiler, not for the transaction engine, not for the Torchship game database (the torchship skills own that), not for in-world verb authoring for a specific game, and not for Thetis's own internals."
universal = false
tags = ["moor", "moo", "objdef", "textdump", "import", "export", "core", "cowbell", "lambdacore", "lambda-moor", "minimal-core", "moorc", "moor-emh", "checkpoint", "load_object", "constants.moo", "bootstrap", "database", "--import", "--import-format", "objdef .moo directories", "import_export_id", "reload_object", "dump_object", "lambdamoo textdump", "toaststunt textdump"]
children = "auto"
related = ["moor/working-in-the-repo/build-and-run", "moor/storage-and-state/world-state-model"]
version = 2
---

# Getting a world into and out of mooR

This skill is written in ASD-STE100 Simplified Technical English.

A mooR server runs from a binary database, not from source files. Source files only
build that database, and exports only read it back out. This topic covers the two
source formats, the one loader interface they share, and the starter databases that
the repository ships.

Read this page to choose a child. Do not act from this page alone.

## Which child to read

- [objdef-format](skill:moor/content-pipeline/objdef-format) — what is in a
  `.moo` object file, how a directory becomes a world, what survives a dump and
  a re-import, and how to load or replace one object in a live world.
- [textdump-compat](skill:moor/content-pipeline/textdump-compat) — reading an
  old LambdaMOO or ToastStunt database, and which textdump features are
  dropped, approximated, or refused.
- [cores-and-bootstrap](skill:moor/content-pipeline/cores-and-bootstrap) — what
  a core is, which bundled one to start from, how a world comes up from
  nothing, and why it starts but no one can log in.

## Common to all three

**Both formats end at the same interface.** `LoaderInterface` in
`crates/common/src/model/loader.rs` is the only way content enters the database.
It is a write interface with no permission checks. `SnapshotInterface`, in the same
file, is the read-only counterpart that every export uses. The database side of both
is in `crates/db/src/api/loader_adapter.rs`, implemented on the same transaction type
that normal world-state work uses.

**A whole-database import is one transaction.** The daemon takes one loader client,
runs the whole import through it, and commits once. There is no partial commit and
no checkpoint part way. Read `moor/storage-and-state/transactions` for what that
commit means.

**An import happens once.** The daemon imports only when it has just created the
database file. On any later start it opens the existing database and ignores the
import path. Editing source files does not change a world that already exists.

**Export is objdef only.** There is no textdump writer anywhere in the tree. The
`--export-format` option is accepted and ignored, and the config comment in
`crates/kernel/src/config.rs` says checkpoints are always objdef.

**The pipeline direction.**

| Stage | Input | Output | Code |
|---|---|---|---|
| Parse | `.moo` text | compiled object definitions | `crates/compiler/src/objdef_literal.rs` |
| Stage | object definitions | one proposed object graph | `crates/objdef/src/set.rs` |
| Apply | proposed graph | database mutations | `crates/objdef/src/load.rs` |
| Collect | database snapshot | object definitions | `crates/objdef/src/dump.rs` |
| Write | object definitions | `.moo` files in a directory | `crates/objdef/src/write.rs` |
| Read textdump | one dump file | in-memory `Textdump` | `crates/textdump/src/read.rs` |
| Load textdump | `Textdump` | database mutations | `crates/textdump/src/load_textdump.rs` |

## One word, one meaning

| Term | Meaning in this topic |
|---|---|
| Objdef | mooR's own format: a directory of `.moo` files, one object each. |
| Textdump | The single-file LambdaMOO 1.8.x format, and its ToastStunt extensions. |
| Core | A starter database: the objects and code that make a world usable. |
| Import | Building a new database from a source format. |
| Export, checkpoint | Writing the live database out as an objdef directory. |
| Conflict (here) | Incoming state differs from state already in the database. Not a transaction conflict. |
| Constant | A symbolic name for an object, declared in `constants.moo`. |
| `import_export_id` | Object metadata that fixes an object's constant name and file name. |

Note the two meanings of *conflict*. A transaction conflict is a commit failure and
belongs to `moor/storage-and-state/transactions`. A loader conflict is a difference
between an objdef and the database, and is handled by a `ConflictMode`.

## Knowledge barriers

Do not change this area before you understand these.

| You must understand | Learn it from |
|---|---|
| The MOO object model: one parent, properties defined once and overridden, verbs resolved by ancestry | `moor/storage-and-state/world-state-model` |
| That a verb is stored compiled, and source is regenerated by decompiling | `moor/language-and-compiler/compiler-pipeline` |
| Which language features are behind feature flags, and that a core needs the right ones | `moor/language-and-compiler/language-features-and-compat` |
| That a task is a transaction, so an in-world load shares the calling task's transaction | `moor/storage-and-state/transactions` |
| The difference between a source directory and the live database | `cores-and-bootstrap` |

## Where the book is behind the code

`book/src/the-system` is the manual for this topic and is worth reading. These
statements no longer match the code, and the code is correct.

| Book statement | The code |
|---|---|
| `server-assumptions-about-the-database.md` lists two checkpoint formats, objdef and textdump | Checkpoints are always objdef. No textdump writer exists. |
| `objdef-file-format.md` tells you to run `make migrate` in a core directory | No core `Makefile` has a `migrate` target. Run `moorc` with `--legacy-type-constants` instead. |
| `cores/lambda-moor/README.md` and its `Makefile` offer a `gen.moo-textdump` target | `moorc` has no `--out-textdump` option. That target fails. |
| `object-loading.md` says a dry run makes no changes | The loader still mutates the transaction it was given. See `objdef-format`. |

## Read next

- [build-and-run](skill:moor/working-in-the-repo/build-and-run) — how to start
  the stack that performs an import.
- [repo-tooling](skill:moor/working-in-the-repo/repo-tooling) — `moorc`,
  `moor-emh` and the other tools.
- [daemon-and-rpc](skill:moor/services/daemon-and-rpc) — the process that owns
  the import and the checkpoint thread.
- [world-state-model](skill:moor/storage-and-state/world-state-model) — the
  model that both formats describe.
