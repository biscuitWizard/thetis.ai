---
name = "The mooR FlatBuffers wire schema"
brief = "How to change a .fbs schema in mooR without breaking a running cluster or an existing database: the generation commands, and the rules for adding, deprecating and never renumbering."
when_to_use = "Use before editing anything under crates/schema/schema/*.fbs (moor_rpc.fbs, common.fbs, var.fbs, task.fbs, moor_event_log.fbs, all_schemas.fbs) or crates/daemon/src/connections/connections.fbs. Use it when regenerating schemas_generated.rs with planus, building TypeScript bindings with flatc or npm run schema:build, adding a field, a union variant or an enum value, deprecating a field, or diagnosing a planus decode error, a missing-field error or a version mismatch between a daemon and a host. Not for the meaning of a particular RPC message (read daemon-and-rpc), not for the MOO Var type itself (read moor/language-and-compiler/value-model), and not for MOO verb code inside a running world such as the Torchship database, which the torchship skills own, or for Thetis internals."
universal = false
tags = ["moor", "flatbuffers", "fbs", "schema", "planus", "flatc", "wire format", "compatibility", "schema evolution", "moor-schema", "generated code"]
version = 1
---

# The mooR FlatBuffers wire schema

`crates/schema` holds every serialised shape in mooR. The `.fbs` files are the
source of truth; the Rust and TypeScript bindings are generated and checked in.
The schema is not only a wire format. It is also the on-disk format of the world
database, the suspended-task database and the event log. That is why the
evolution rules below are strict.

## Why FlatBuffers

Three constraints forced it.

1. **Other languages must be able to speak the protocol.** A worker or a client
   may be written in Python, TypeScript or anything else. `tools/example-python-worker`
   exists to prove that. A Rust-only encoding would close the system.
2. **The daemon reads messages on hot paths.** FlatBuffers is read in place, so
   the daemon can pull one field out of a request without parsing the rest. Much
   of the daemon works on `...Ref` types and never builds an owned value.
3. **Stored data must survive a version change.** A table with appended fields
   stays readable by old and new code, which is what makes a rolling upgrade and
   an old database possible at all.

The cost is that the schema is a contract with data already written to disk. You
cannot "just rename that field".

## What is where

| File | Contains | Also stored on disk in |
|---|---|---|
| `crates/schema/schema/common.fbs` | `Obj`, `Symbol`, `Uuid`, `ObjectRef`, errors, verb and property definitions | The world database |
| `crates/schema/schema/var.fbs` | The `Var` union: every MOO value | The world database |
| `crates/schema/schema/moor_program.fbs` | Compiled program representation | The world database |
| `crates/schema/schema/task.fbs` | Suspended task state | The persistent tasks database |
| `crates/schema/schema/moor_event_log.fbs` | Logged narrative events and presentations | The event log database |
| `crates/schema/schema/moor_rpc.fbs` | Every RPC message, reply, event and error | Nothing (wire only) |
| `crates/schema/schema/all_schemas.fbs` | The master file that includes the others | — |
| `crates/daemon/src/connections/connections.fbs` | Connection records; daemon-private, includes the shared files by relative path | The connections database |

`moor_rpc.fbs` is the only one you can change with wire compatibility as the sole
concern. Everything else has a database behind it.

Note that `var.fbs` distinguishes what may be *transmitted* from what may be
*persisted*: a lambda is legal in the database encoding and refused on the wire.
Keep that distinction when you add a variant.

## Generating the bindings

Both generators are external tools. Neither runs in the build, and neither runs
in CI as a freshness check, so a stale checked-in file is possible.

**Rust.** From `crates/schema/schema`:

    planus rust -o ../src/schemas_generated.rs all_schemas.fbs

Commit `crates/schema/src/schemas_generated.rs` in the same change as the `.fbs`
edit. Use the planus version the workspace depends on; the generated file asserts
compatibility with a specific planus version at compile time, so a mismatched
generator fails the build with a version message rather than silently.

**TypeScript.** From the repository root:

    npm run schema:build

This needs `flatc`; set `MOOR_FLATC` if it is not on `PATH`. The CI web job pins
the flatc version it installs — read `.github/workflows/ci.yml` for the current
one. The TypeScript output is build output and is not committed. Import generated
types from their namespace module, never from the master `all_schemas_generated.ts`,
which has colliding export names and is excluded from the build.

**Dart.** The Flutter client generates a third set of bindings with
`clients/meadow_flutter/tool/gen_flatbuffers.sh`, rooted at `moor_rpc.fbs` with
`--gen-all`. The script refuses to run unless `flatc` is exactly the pinned
version, which is the same one CI installs. Its output is committed under the
Flutter client. So a schema change has **three** binding sets to consider, not
two, and only the Rust one is committed inside `crates/schema`.

Run the planus command **from `crates/schema/schema`**. A run from the repository
root writes to a path that looks plausible and is wrong; there is already a stray
`crates/schema/crates/schema/src/schemas_generated.rs` in the tree from exactly
that mistake. Nothing includes it. Do not edit it, and do not mistake it for the
real generated file.

## The conversion layer

Generated types are not used directly by most code. `crates/schema/src` wraps
them:

| Module | Role |
|---|---|
| `lib.rs` | Re-exports the generated namespaces as `common`, `var`, `rpc`, `task`, `event_log`, `program` |
| `convert_*.rs` | Hand-written conversions between MOO domain types and schema types, re-exported as `moor_schema::convert` |
| `macros.rs` | `fb_read!` and friends: read a required field and turn a planus error into a message naming the field |
| `crates/schema-macros` | `EnumFlatbuffer` derive and `define_enum_mapping!`, which generate both directions of a simple enum mapping |

When you add an enum variant, the derive is what tells you every place that must
learn about it. Add the variant to the `.fbs`, regenerate, then let the compiler
find the non-exhaustive matches. Do not add a catch-all arm to silence it.

`moor_runtime_api::api_codec` is the other half: it converts the typed request,
reply and event enums in `moor_runtime_api::api` to and from FlatBuffers. The
daemon's own logic never sees a FlatBuffer.

## Evolution rules

These are the rules that decide whether a change is safe. Break one and you break
either a running deployment or a database that already exists.

**You may:**

1. **Append a new field at the end of a table.** Old readers ignore it. Give it a
   default, or make it optional in the Rust conversion.
2. **Append a new variant at the end of a union.** Old readers see an unknown
   variant and must handle it; check that the receiving `match` in
   `convert_*` or `api_codec` degrades rather than panics.
3. **Append a new value to an enum with a new, higher number.**
4. **Add a whole new table, union or root message.**
5. **Mark a field `(deprecated)`.** This keeps the wire slot reserved forever and
   removes the accessor, which is what you want. `moor_rpc.fbs` already does this
   for `LoginCommand.event_log_pubkey`, with a comment saying the slot is
   retained for compatibility. Copy that pattern, comment included.

**You must never:**

6. **Renumber or reorder an enum value.** Enum numbers are stored in the world
   database and in the event log. Renumbering silently reinterprets existing
   data. This is the single most damaging change you can make here.
7. **Reorder fields inside a table, or reuse a removed field's slot.** Field
   order in the file is the vtable order.
8. **Delete a field.** Deprecate it instead. Deletion shifts every later slot.
9. **Change a field's type.** Add a new field and migrate.
10. **Add `(required)` to an existing optional field.** Every message and every
    stored record written before the change fails to decode.
11. **Remove `(required)` from an existing field** without auditing every reader,
    because readers were entitled to assume it was present.
12. **Reorder union variants.** The variant tag is the position.

**Rename with care.** A rename does not change the wire, but it does change the
generated Rust and TypeScript names, so it is a source-level break in every
downstream crate and in `clients/web-sdk`. It is safe for the data and expensive
for the tree. Say so in the commit.

## What a mistake actually breaks

| Change | Breaks |
|---|---|
| Renumbered `ErrorCode` | Every stored MOO error value in the world database now means something else |
| Renumbered or reordered `VarUnion` | The entire world database becomes unreadable |
| Reordered fields in `LoggedNarrativeEvent` | Every already-written history record decodes to nonsense; the payload is encrypted so no test will notice until a user scrolls back |
| Added a `moor_rpc` field but did not regenerate | The daemon and the host disagree; the daemon replies "Could not decode request body" |
| Regenerated Rust but not the TypeScript bindings | Meadow and the web SDK break at runtime, not at build time |
| Regenerated Rust and TypeScript but not the Dart bindings | The Flutter client breaks, and nothing in the npm or cargo build notices |
| Reordered `HostClientToDaemonMessageUnion` | Every host at the old version sends what the daemon reads as a different message |

## Invariants

1. **The `.fbs` file is the source of truth. The generated file is a build
   artefact that happens to be committed.** Never hand-edit
   `schemas_generated.rs`.
2. **A schema change and its regenerated Rust land in the same commit.** Nothing
   in CI checks that they agree.
3. **A field slot, once used, is used forever.** Deprecate; do not delete.
4. **An enum number is permanent.** It is stored on disk.
5. **A change to `common.fbs`, `var.fbs`, `moor_program.fbs`, `task.fbs` or
   `moor_event_log.fbs` is a database migration question, not a protocol
   question.** Ask what happens to an existing `moor-data` directory before you
   ask what happens to a running host.
6. **All daemon-side reads go through `fb_read!` or an explicit error.** A
   missing field must produce a named error, never a panic and never a silent
   default.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Build fails on `check_version_compatibility("planus-x.y.z")` | The committed generated file came from a different planus | Install the planus version matching the workspace `planus` dependency and regenerate |
| "Failed to read `<field>`" at runtime | A required field the sender did not set, or a version skew | Compare the `.fbs` on both sides. `fb_read!` names the field for you |
| Daemon replies "Could not decode request body" | The peer is built against a different schema | Rebuild every process from one checkout. There is no negotiated protocol version |
| A new field is always the default | The sender was not updated, or the field was appended in the wrong place | Confirm it is the last field of the table and that the builder sets it |
| TypeScript build fails with duplicate exports | Something imported `all_schemas_generated.ts` | Import from the namespace module instead |
| `npm run schema:build` cannot find `flatc` | Not installed or not on `PATH` | Install it, or set `MOOR_FLATC` to its absolute path |
| An old database will not open after a schema edit | An enum was renumbered or a field reordered | Revert the schema change. There is no repair once data is written with mixed encodings |

## Read first / read next

- Read `moor/language-and-compiler/value-model` before changing `var.fbs`. The
  `Var` union mirrors the runtime type system and must stay aligned with it.
- Read `moor/storage-and-state/storage-engine` before changing anything that the
  world database stores.
- Read `daemon-and-rpc` for what the messages in `moor_rpc.fbs` mean.
- Read `clients-and-web-ui` for who consumes the TypeScript and Dart bindings and
  what a client is entitled to assume when the schema grows.
- `crates/schema/schema/README.md` holds the exact generation commands and the
  licensing note (the `.fbs` files are LGPL, unlike the rest of the server).
- `doc/messaging.md` describes the schema directory as `crates/common/schema/`
  and lists a `db.fbs`. Both are out of date; the code is at
  `crates/schema/schema/` and there is no `db.fbs`.
