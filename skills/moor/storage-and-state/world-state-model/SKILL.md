---
name = "The world-state model"
brief = "How mooR stores objects, properties, verbs and inheritance as relations, how name resolution walks ancestry, and which layer checks permissions."
when_to_use = "Use when you must know where an object attribute, property, verb program or permission bit actually lives, when adding a relation, or when a lookup or resolution cache is wrong. Not for transaction conflict and retry, the on-disk format, or object lifecycle, which the sibling skills own, and not for the Torchship database."
universal = false
tags = ["moor", "worldstate", "loaderinterface", "snapshotinterface", "dbworldstate", "taskpermissions", "relations", "objects", "properties", "verbs", "inheritance", "parent", "propdef", "verbdef", "permissions", "resolution cache", "ancestry", "moor-db"]
version = 2
---

# The world-state model

The world is a set of typed key-to-value relations, not a graph of records. Every
object attribute, every property value and every verb program is one tuple in one
relation. This page states the decomposition, the resolution rules that sit on top of
it, and the boundary where permissions are checked.

## The relations

`define_relations!` in `crates/db/src/engine/moor_db.rs` is the single declaration.
Read it before you believe any list, including this one.

| Relation | Key | Value | Holds |
|---|---|---|---|
| `object_flags` | object | flag bits | Existence, and the player, programmer, wizard, read, write, fertile bits |
| `object_name` | object | string | The object's name |
| `object_owner` | object | object | The owner. Secondary indexed. |
| `object_parent` | object | object | The single parent. Secondary indexed. |
| `object_location` | object | object | The container. Secondary indexed. |
| `object_verbdefs` | object | verb definition set | Every verb defined *on* this object: names, owner, flags, argument spec |
| `object_verbs` | (object, uuid) | program | The compiled verb program |
| `object_propdefs` | object | property definition set | Every property *defined* on this object |
| `object_propvalues` | (object, uuid) | value | One property value held by one object |
| `object_propflags` | (object, uuid) | owner and flags | Property permissions for one holder |
| `entity_metadata` | (tag, object, uuid, key) | value | Side metadata on an object, property or verb |
| `object_last_move` | object | value | The last move record |
| `anonymous_object_metadata` | object | timestamps | Creation and last-access time of an anonymous object |

Four facts follow from this table and matter more than the table itself.

**Existence is a row in `object_flags`.** `valid()` asks whether that key is present.
Nothing else defines whether an object exists.

**There is no contents relation and no children relation.** Contents is the reverse
lookup of `object_location`. Children is the reverse lookup of `object_parent`. Owned
objects is the reverse lookup of `object_owner`. The `==` marker in the relation
declaration means the index carries a value-to-keys map as well. Deleting the forward
tuple removes the reverse entry; do not try to maintain both.

`crates/db/src/config.rs` still accepts `object_contents` and `object_children` table
settings. They configure keyspaces that no relation uses. Do not read them as
evidence that those relations exist.

**A property definition and a property value are different rows.** The definition
exists once, on the object that defined it, inside that object's `object_propdefs`
set. A value exists per holder, keyed by the holder object and the definition's UUID.
An object with no value row for a UUID is *clear* for that property.

**A verb definition and a verb program are different rows.** The definition lives in
the definer's `object_verbdefs` set. The compiled program is keyed by the definer and
the definition's UUID.

## How resolution works

Both property and verb lookup walk the parent chain from the object upward. The
UUID in a definition is the stable identity that survives that walk.

**Property resolution.** Find the nearest ancestor whose `object_propdefs` names the
property; that gives the UUID and the definer. Then read the value for the *starting*
object and that UUID. If there is no value row, the property is clear, and the search
continues up the ancestors for the nearest holder that does have a value. If none
does, the result is none, and the caller is told the value was inherited.

**Property permissions** come from the `object_propflags` row for the holder that
supplied the definition. They are not re-derived per holder.

**Verb resolution.** Walk from the object upward. On each object, search that
object's verb definition set by name, then test the argument specification and the
required flags. The first match wins. A name match whose specification does not match
does not stop the walk, and does not record a negative result.

**Duplicate names are refused at definition time.** `define_property` rejects a name
that already exists on the object, on any ancestor, or on any descendant. This keeps
one name mapped to one UUID for a whole branch.

## Why the caches exist, and what they cost

An unaided lookup is a walk up the ancestry, with a set search at each level. A busy
world does this many times per verb call. Three caches remove most of that work.

| Cache | Answers | Also caches |
|---|---|---|
| `PropResolutionCache` | (object, name) to property definition | Negative results, and the first ancestor that has any property definitions |
| `VerbResolutionCache` | (object, name) to resolved verb | Negative results, and the first ancestor that has any verb definitions |
| `AncestryCache` | object to its ancestor list | Membership tests for `isa` |

They are not ordinary caches. They are versioned along with the world:

- Each transaction **forks** the published caches at start, so a transaction sees only
  its own invalidations plus what was published before it started.
- A committing transaction publishes its forked caches with its snapshot. A read-only
  transaction may publish cache updates alone, through a separate atomic plane, and
  only if the root has not moved.
- Any structural change invalidates. Reparenting, defining or deleting a property or
  verb, recycling, and renumbering all invalidate the affected object and, where the
  change is structural, the whole descendant branch.

The rule for a change: **if you add a code path that changes ancestry, a property
definition set, or a verb definition set, you must invalidate.** A missed
invalidation does not fail a test on the same object. It shows up later as a verb or
property that resolves to a stale definition, or resolves on an object that should no
longer have it.

## The API boundary and permissions

Three traits sit over the same transaction. They differ in exactly one respect: who
checks permissions.

| Trait | Implemented by | Permissions |
|---|---|---|
| `WorldState` | `DbWorldState` in `crates/db/src/api/world_state.rs` | **Checked.** Almost every method takes a `TaskPermissions`. |
| `LoaderInterface` | The same adapter, in `crates/db/src/api/loader_adapter.rs` | **Not checked.** No method takes permissions. |
| `SnapshotInterface` | `FjallSnapshotLoader` | Read-only, no permissions, and reads on-disk state, not the live transaction. |

Below all three, `WorldStateTransaction` in `crates/db/src/engine/ws_transaction.rs`
performs no permission check at all. It is the raw relational surface. It is not
public outside the crate, and it must stay that way.

What a caller must supply for `WorldState`:

- A `TaskPermissions`, which carries the principal object, that object's cached flags,
  and any additive capability grants for the activation.
- The principal is the object the activation runs as. It starts as the verb owner and
  the MOO `set_task_perms` builtin changes it.

`crates/db/src/api/auth.rs` holds the rules. They are stated as authorization shapes,
not as operation names: an object rule denies with an object permission error, a
property rule with a property permission error, a verb rule with a verb permission
error. A call site resolves the owner and flags first, then asks for the matching
rule. Add a rule there rather than writing a bespoke check at a call site.

`moor/execution/permissions-and-security` owns the meaning of the flags and of the
capability grants. This skill only fixes where they are enforced.

## Object identity

An `Obj` is a 64-bit value whose top two bits give the kind.

| Kind | Made by | Notes |
|---|---|---|
| Numbered | `ObjectKind::Objid` or `NextObjid` | The classic `#123`. Allocated from a shared sequence. |
| UUID | `ObjectKind::UuObjId` | Generated, not sequential. Avoids number exhaustion. |
| Anonymous | `ObjectKind::Anonymous` | Has no stable printable identity and is garbage collected. See `object-lifecycle-and-gc`. |

All three kinds are keys in the same relations. Nothing in the storage layer treats a
UUID object differently from a numbered one. Only the allocator and the collector
care.

## Invariants

1. **One parent per object.** `object_parent` is a map, not a multimap. Ancestry
   walks assume a chain, and they stop at `NOTHING` or at a self-loop.
2. **A property name is unique across a branch.** Enforced at definition time. If a
   change lets two definitions with one name coexist in a branch, resolution becomes
   order-dependent and the caches will disagree with the relations.
3. **A property value row implies a property definition row on some ancestor.** A
   value with no reachable definition is unreachable data. Deleting a definition must
   delete the values.
4. **Reverse lookups are derived, never stored.** Do not add a contents or children
   relation. Maintaining two truths creates a class of bug that has no failing test.
5. **Permissions are checked in `DbWorldState` and nowhere below it.** A new engine
   method is unchecked by definition. Expose it through the adapter with a rule.
6. **A cache must never be more optimistic than the relations.** On any doubt,
   invalidate. A false negative costs a walk. A false positive is a security and
   correctness fault.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A verb resolves after it was deleted, or on the wrong object | Missing verb cache invalidation | Find the mutating path and invalidate the object, or the branch for a structural change |
| A property reads as clear after a value was set | The value row was written under the wrong holder key | The key is (holder, definition UUID), not (definer, UUID) |
| `DuplicatePropertyDefinition` on a name you do not see | The name exists on an ancestor or a descendant | Search the whole branch, not just the object |
| A wizard-only operation succeeds for a non-wizard | The call site used the engine transaction, or skipped the auth rule | Route it through `DbWorldState` and an `AuthRule` |
| An import writes things a player could not | Expected. The loader adapter has no permission checks | Only trusted import and bootstrap code may hold a `LoaderInterface` |
| `children` or `contents` looks stale | A forward tuple was deleted without going through the relation API | Delete through the relation so the secondary index updates |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/db/src/engine/moor_db.rs` | The relation declaration. The authority for the relation list. |
| `crates/db/src/engine/ws_transaction.rs` | The unchecked relational operations, and the resolution walks |
| `crates/db/src/api/world_state.rs` | The `WorldState` adapter, with permission checks and counters |
| `crates/db/src/api/auth.rs` | The authorization rules and the resolved principal |
| `crates/db/src/api/loader_adapter.rs` | The unchecked loader and snapshot surfaces |
| `crates/db/src/cache/` | The verb, property and ancestry caches, and their statistics |
| `crates/common/src/model/world_state.rs` | The `WorldState` and `WorldStateSource` traits, and `WorldStateError` |
| `crates/common/src/model/loader.rs` | The `LoaderInterface` and `SnapshotInterface` traits |
| `crates/common/src/model/propdef.rs`, `verbdef.rs`, `defset.rs` | The definition types and the immutable set that holds them |
| `crates/common/src/model/task_permissions.rs` | `TaskPermissions` and the capability grants |
| `crates/var/src/obj.rs` | The `Obj` encoding and its three kinds |
| `crates/db/benches/` | Benchmarks for the property, verb and relation paths |

## Read first, read next

Read `transactions` first if you do not yet know how a working set and a snapshot
relate; the cache forking rules only make sense afterwards. Read
`object-lifecycle-and-gc` for what a create or recycle does to these relations. Read
`storage-engine` for how a relation becomes bytes. Read
`moor/execution/permissions-and-security` for what the flags mean, and
`moor/language-and-compiler/value-model` for what a property value can hold.
