---
name = "Importing a LambdaMOO textdump"
brief = "What mooR's textdump import really promises: which dump versions load, what is preserved or dropped, and the checklist for bringing an old MOO database across."
when_to_use = "Use when an existing LambdaMOO 1.8.x, ToastStunt or Stunt database must run on mooR, or an import stops on a verb that will not compile. Not for mooR's own objdef directory format (read objdef-format), not for exporting (mooR writes no textdump), not for choosing a core (read cores-and-bootstrap), and not for the Torchship game database or Thetis's own internals."
universal = false
tags = ["moor", "textdump", "lambdamoo", "toaststunt", "stunt", "import", "legacy", "migration", "waif", "compatibility", "JHCore", "LambdaCore", "moor-textdump", "continue-on-errors", "--import-format", "--src-textdump", "anonymous objects", "forked and suspended tasks", "iso-8859-1"]
related = ["moor/language-and-compiler/language-features-and-compat", "moor/content-pipeline/objdef-format"]
version = 2
---

# Importing a LambdaMOO textdump

This skill is written in ASD-STE100 Simplified Technical English.

A textdump is the single-file database format that LambdaMOO 1.8.x wrote, and that
ToastStunt extended. The `moor-textdump` crate in `crates/textdump` reads one and
feeds it through the same `LoaderInterface` that objdef uses. This is a one-way
bridge: mooR reads a textdump and never writes one.

## What compatibility means here

The promise is narrow and worth stating exactly.

- mooR **reads** the LambdaMOO textdump grammar and most of the ToastStunt one.
- mooR **compiles** the verb source that a dump carries, with its own compiler.
- mooR **does not** promise that the resulting world behaves the same. A verb that
  compiled under LambdaMOO can be rejected by mooR, and a verb that compiles can
  still fail at run time.
- mooR **never writes** a textdump. There is no writer in the tree, and the
  `--export-format` option is ignored. A world that comes in through a textdump
  leaves through an objdef export.

Treat a textdump import as a migration with manual work after it, not as a load.

## Which dumps are accepted

The reader identifies the dump from its first line, in `crates/textdump/src/lib.rs`.

| Version line | Treatment |
|---|---|
| LambdaMOO format versions up to and including 4 | Accepted. |
| LambdaMOO format version above 4 | Read as ToastStunt, with a warning that unsupported features may appear. |
| ToastStunt versions | Accepted with a warning. The layout differs: users, pending finalization values, the task queue and connections come before the objects. |
| A mooR version line | Accepted when the major semantic version matches the running server. Minor and patch differences are allowed. |
| Anything else | Refused as a version error. |

The exact version enumerations, including which ToastStunt feature each version
number introduced, are the `LambdaMOODBVersion` and `ToastStuntDBVersion` enums in
`crates/textdump/src/lib.rs`. Read them there rather than from any list.

Text encoding follows the version. A LambdaMOO or ToastStunt dump is decoded as
ISO-8859-1. A mooR dump uses the encoding named in its own version line.

## Preserved, approximated, dropped, refused

| Category | Items |
|---|---|
| Preserved | Object numbers, names, flags, parent, location, owner. Property definitions, values, owners and flags. The clear/non-clear state of a property. Verb names, argument specification, owner and permission bits. Verb source, recompiled. The user list. |
| Approximated | ToastStunt anonymous objects, which become mooR anonymous objects with newly generated identities. Custom error values, which become mooR custom errors. Verb permission bits; see the hazard below. |
| Dropped, with a warning | WAIFs, which become the `none` value wherever they appear. Every WAIF site is logged with its object, property and line. ToastStunt "pending finalization" values. Recycled object entries. Formerly active connections. |
| Dropped, silently or nearly so | Forked, suspended and interrupted tasks. Each group is counted and logged, then skipped. Task state cannot be migrated. |
| Refused | An unsupported dump version. An unsupported preposition or argument specifier in a verb header. An unrecognised error code. |

Two structural notes. Property *inheritance* is reconstructed by walking the parent
chain: a dump lists a child's property values positionally, and the reader resolves
each position back to its definer. Clear values stay clear. And ToastStunt dumps
carry a second block of anonymous objects after the main object list, which the
reader parses and discards.

## The load path

`textdump_load` opens the file, `read_textdump` parses it whole into memory, and then
the loader applies it. The application is multi-pass for the same reason objdef's is:
everything must exist before anything can point at it.

| Pass | What it does |
|---|---|
| 1 | Create every object with its flags and name, and with no parent, location or owner. |
| 2 | Set owner, parent and location. Parent validation is off. |
| 3 | Define each property at the object that defines it. |
| 4 | Set every property value, owner and flags, including clear state. |
| 5 | Convert verb flag bits and argument specifiers, compile each verb, add it. |
| 6 | Derive `import_export_id` metadata from the properties on `#0`. |

Pass 6 is the bridge to objdef. Every property defined on `#0` whose value is a valid
object gives that object a stable name, and `#0` itself is named `sysobj`. This is
what makes a later objdef export produce `constants.moo` entries and named files
instead of numbers. It is the reason the standard migration route is: import the
textdump, then export objdef, then keep the objdef in version control.

Like objdef, a textdump import turns on `call_unsupported_builtins`, so a call to a
function mooR does not have compiles and fails only when the verb runs.

## Verbs that will not compile

This is the common failure, and it has one switch.

By default any verb that does not compile aborts the whole import. With
`--continue-on-errors` on `moorc`, or `continue_on_compile_errors` in
`TextdumpImportOptions`, the verb is created with an empty program, a warning names
it, and the import continues. The count of failed verbs is logged at the end.

One case gets its own message: assignment to a type literal, such as assigning to
`INT`. That is legal in LambdaMOO and ToastStunt and is rejected by mooR. It always
needs a manual edit. Related to this, `moorc --legacy-type-constants true` makes the
old bare type constants parse, and writes the new `TYPE_*` spelling on export.

An empty program is not a working verb. Treat the warning list as the work queue.

## Checklist for bringing an old database across

1. **Decide the feature flags first.** `use_boolean_returns`, `use_symbols_in_builtins`,
   `custom_errors`, `use_uuobjids` and `anonymous_objects` change what old code does.
   An old core usually wants them off; `cores/lambda-moor` is a LambdaCore that was
   edited until it works with them on. The flag set is `FeaturesConfig` in
   `crates/vm/src/config.rs`.
2. **Import with a tool, not with the server, on the first attempt.** `moorc
   --src-textdump <file> --out-objdef-dir <dir>` converts and validates without
   creating a server database. Add `--continue-on-errors` to get the full list of bad
   verbs in one run instead of one per run.
3. **Read the warnings.** WAIF sites, failed verbs, and the skipped task counts are
   the whole manual work list.
4. **Repair the failed verbs in the objdef output**, then import the objdef directory
   instead of the textdump from then on.
5. **Expect to lose queued work.** Anything the old server had forked or suspended is
   gone. Anything that depended on it must be restarted by a `server_started` hook.
6. **Check the login path before you announce the world.** See
   [cores-and-bootstrap](skill:moor/content-pipeline/cores-and-bootstrap).
7. **Do not plan to go back.** There is no export to textdump.

## Invariants

1. The reader never writes. Everything it produces goes through `LoaderInterface`.
2. A whole textdump is parsed into memory before any object is created. A large core
   costs memory proportional to the dump.
3. A textdump import is one transaction. It commits once, or the database it created
   is deleted.
4. Object numbers from the dump are preserved exactly. The importer never renumbers.
5. Task state is never migrated.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/textdump/src/lib.rs` | Version detection, encoding modes, the verb flag constants, the in-memory dump types. |
| `crates/textdump/src/read.rs` | The parser: objects, properties, verbs, values, lambdas, WAIF skipping, task queue skipping. |
| `crates/textdump/src/load_textdump.rs` | The passes, verb compilation policy, and the `#0` to `import_export_id` bridge. |
| `crates/textdump/tests/textdump.rs` | The read tests, including a full JHCore import. |
| `crates/daemon/src/lib.rs` | `perform_import`, which selects the format and owns the transaction. |
| `tools/moorc/src/main.rs` | `--src-textdump`, `--continue-on-errors`, `--legacy-type-constants`. |
| `cores/JHCore-DEV-2.db` | The bundled large textdump used as the import test fixture. |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| `Unsupported LambdaMOO DB version` | The version line is outside the accepted range | Check the first line of the dump. A dump from a fork mooR does not know cannot be read. |
| `Incompatible major moor version` | A mooR-written dump from a different major release | Only the major version must match. Use the matching server, or convert with a build of that major version. |
| Import aborts on one verb | Default compile policy | Re-run with `--continue-on-errors` to collect every bad verb at once, then repair them. |
| `assignment to type literal ... valid in LambdaMOO/ToastStunt, but not in mooR` | Old code assigns to a bare type constant | Edit the verb. `--legacy-type-constants` makes the constants parse but does not make the assignment legal. |
| Warnings about WAIF values | The dump is from ToastStunt and uses waifs | Every site is listed. Each becomes `none`. Replace them by hand or drop the feature. |
| "Skipping N queued tasks" and similar | Task state is not migrated | Expected. Restart the work from a `server_started` hook. |
| The daemon panics part way through an import | The loader path uses direct unwraps on several database calls, so a structurally inconsistent dump aborts the process rather than returning an error | Import with `moorc` first, where the failure is contained, and check the last log line for the object being processed. |
| Verbs come across with the debug flag set unexpectedly | The verb flag constants are decimal in `crates/textdump/src/lib.rs`, and the debug mask overlaps the write bit | Compare the imported verb permissions against the source database before you trust them. |
| An imported world exports to numbered files with no constants | `#0` had no object-valued properties, so no `import_export_id` was derived | Set the sysobj properties, or add `import_export_id` metadata, before the first export. |

## Read first, read next

Read first:

- [language-features-and-compat](skill:moor/language-and-compiler/language-features-and-compat)
  — which language differences will bite an old core.
- [world-state-model](skill:moor/storage-and-state/world-state-model) —
  property definition, override and clear state, which the reader reconstructs
  by hand.

Read next:

- [objdef-format](skill:moor/content-pipeline/objdef-format) — the format an
  imported world should be moved to.
- [cores-and-bootstrap](skill:moor/content-pipeline/cores-and-bootstrap) —
  what the server needs the imported database to contain.
- [repo-tooling](skill:moor/working-in-the-repo/repo-tooling) — running
  `moorc`.
