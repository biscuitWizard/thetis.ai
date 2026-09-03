---
name = "mooR task permissions and the trust model"
brief = "Who a MOO task runs as, what the wizard and programmer bits grant, how permissions change across a verb call, what capability grants add, and where no check is applied at all."
when_to_use = "Use when reasoning about authority in the mooR server: TaskPermissions, the wizard and programmer and owner bits, the r/w/f object flags and the r/w/c property flags and the r/w/x/d verb flags, set_task_perms and caller_perms and task_perms, capability grants such as property_read or verb_call or builtin_call, E_PERM from a property or verb access, or whether a given operation is checked at all. Use it before adding or changing any permission check. Do not use it for the scheduler, the opcode loop, the mechanics of writing a builtin, or command parsing. Do not use it for the database storage engine, for the Torchship MOO game database, for in-world MOO verb authoring, or for Thetis's own internals."
universal = false
tags = ["moor", "permissions", "security", "wizard", "programmer", "set_task_perms", "caller_perms", "capability grants", "E_PERM", "verb flags", "property flags", "owner", "authority"]
version = 1
---

# mooR task permissions and the trust model

Every operation that touches the world state is authorised against one object:
the task's **authority principal**. The model is LambdaMOO's — owner, wizard bit,
programmer bit, and per-resource flags — plus one mooR extension, capability
grants.

Read `moor/storage-and-state/world-state-model` first. The checks live at the
world state boundary, not in the VM.

## The one type that matters

`TaskPermissions` in `crates/common/src/model/task_permissions.rs` carries three
things: the principal, that principal's object flags as they were cached, and a
set of capability grants. Every world state method that can be denied takes one.

There is no ambient authority. Kernel code that wants to bypass a check
constructs a `TaskPermissions` for `#0`, which has no flags set and therefore is
**not** a wizard. Reading server options works that way and succeeds only because
the properties it reads are readable.

## Who a task runs as

| Stage | Principal |
|---|---|
| Task submission | The player, for a command or an eval. The caller-supplied authority principal, for an RPC verb invocation. |
| Entering a verb | The **owner of the resolved verb**, not the caller and not the player. |
| Inside a verb, after `set_task_perms(x)` | `x`, for the rest of that activation. |
| Inside a builtin | The nearest enclosing MOO activation's principal. Builtin frames are transparent. |
| A forked task | The forking activation's principal, carried in the fork record. |

The single most common mistake is confusing three different objects:

- **`player`** is the connection identity. It is used for output, for command
  parsing, and for the `player` variable. It is not an authority.
- **`caller_perms()`** is the principal of the activation *below* the current one.
  It is what a wizard-owned verb tests to decide whether its caller is trusted. It
  is **not** what the database checks.
- **the task authority** is what the database checks. It is the current verb's
  owner unless `set_task_perms` changed it.

A verb owned by a wizard runs with wizard authority no matter who called it. That
is the whole basis of privilege in a MOO core, and it is why the `x` flag and
verb ownership matter as much as they do.

## The bits

| Bit | On | Grants |
|---|---|---|
| Wizard | An object | Passes every owner check, every explicit wizard check, and every flag check. It is total authority over the database. |
| Programmer | An object | Permission to compile and install code: `eval`, and programming a verb. It grants nothing over data. |
| `r`, `w`, `f` | An object | Public read, public write, and fertility (may be used as a parent). |
| `r`, `w`, `c` | A property | Public read, public write, and chown: who owns a descendant's copy of the property. |
| `r`, `w`, `x`, `d` | A verb | Public read of the code, public write, executability, and whether errors raise. |

The general rule for a flagged resource is: **the principal owns it, or the
principal is a wizard, or the resource's flag is set, or a capability grant covers
it.** Any one suffices.

Two departures from that rule are worth knowing:

- Programming a verb is checked against the wizard bit or the **programmer** bit,
  not against ownership. Writing the verb definition is a separate check. A task
  can hold one without the other, and `set_verb_code()` needs both.
- Changing the owner of a property or a verb is wizard-only. A non-wizard may
  leave the owner unchanged and nothing else.

A non-programmer's `eval` does not fail with a permission error at the check
site. The eval setup replaces the program with one that returns `E_PERM`.

## How permissions travel through a verb call

Two different principals are used at two different moments.

1. **Lookup** uses the caller's permissions. The verb is resolved up the
   inheritance chain, and method dispatch requires the `x` flag as part of the
   lookup filter.
2. **Execution** uses the resolved verb's owner, with the flags dispatch selected.

Because the `x` flag is part of the method lookup filter, a verb without `x` is
not found at all. The error a caller sees is `E_VERBNF`, not `E_PERM`. Do not read
a "verb not found" as evidence that the verb does not exist. Command dispatch is
the exception: it resolves without the filter and then authorises, so a command
verb without `x` denies instead. See `command-parsing`.

Nothing else travels. In particular:

- `set_task_perms` affects the current activation and any builtin frames above
  it. A verb called afterwards gets its own owner's authority.
- **Capability grants do not cross a verb call.** A new activation is built with
  an empty grant set. A granted operation that dispatches into MOO code — `move()`
  calling `:accept`, `recycle()` calling `:exitfunc` — runs that code without the
  grant.

## Capability grants

The two-argument form of `set_task_perms` is wizard-only and attaches a list of
narrow, additive rights to the current permissions. They do not replace the flag
model; they satisfy one specific check each.

The grant kinds, as the code parses them:

| Domain | Grants |
|---|---|
| Object | `object_read`, `object_write`, `object_rename`, `object_move`, `object_recycle`, `object_chparent`, `object_list` |
| Property | `property_read`, `property_write`, `property_define`, `property_delete` |
| Verb | `verb_read`, `verb_write`, `verb_add`, `verb_program`, `verb_call` |
| Builtin | `builtin_call` |

The intended pattern: a wizard-owned helper verb validates something at a higher
level — a token, a policy record, a capability object — then drops to the player's
identity plus exactly the rights the rest of the helper needs. The rest of the
helper then runs with the player's visibility everywhere else.

Three properties to keep in mind:

- **A verb grant binds to a verb definition, not to a name.** The name is resolved
  when the grant is created. Renaming the verb keeps the grant; giving the old name
  to a different verb does not move it.
- **`verb_call` resolves through normal method dispatch**, so it follows
  inheritance and wildcard names at the moment the grant is made. It also lets
  dispatch find a verb that lacks the `x` flag, which nothing else does.
- **`builtin_call` is a call-surface grant only.** It satisfies that one builtin's
  own wizard-or-owner check. It does not make the task a wizard, and the
  lower-level object, property, and verb checks inside that builtin still apply. A
  `builtin_call` grant for `set_task_perms` does not let non-wizard code install
  grants.

`task_perms()` returns the current principal followed by the active grants. A
grant bound to a verb that has since been deleted is omitted.

## Where no check is applied

These are deliberate LambdaMOO compatibility choices. Treat each as a trap, not a
bug, and do not "fix" one without understanding what depends on it.

| Operation | Check |
|---|---|
| `valid()` | None. |
| Object owner, and object flags | None. Anyone can see who owns anything and whether it is a wizard. |
| `location`, `contents` | None. A permissions argument is accepted and ignored. |
| `parent`, `children` | None. |
| Object `name` | None on read. Writing it needs the owner, the wizard bit, or a rename grant — and the wizard bit alone if the object is a player. |
| The pseudo-properties `programmer`, `wizard`, `r`, `w`, `f` | None on read. They report object flags. |
| Object enumeration | Wizard, or an `object_list` grant. |

Everything else on the world state does check: ordinary property read and write,
the verb and property listings, verb code retrieval, creation, recycling, moving,
chparent, and flag changes.

Writing `location`, `contents`, `parent`, or `children` as if they were ordinary
properties is refused outright, whatever the authority. Use `move()` and
`chparent()`.

## Invariants

1. **Authorisation happens at the world state boundary, not in the VM.** Do not
   add a permission test inside the interpreter. Add an `AuthRule` and require it
   at the world state call site.
2. **Every denial names its domain.** Object rules deny with an object permission
   error, property rules with a property permission error, verb rules with a verb
   permission error. The kernel maps those to `E_PERM`. Do not blur them.
3. **A verb activation runs as the verb's owner.** Nothing may push an activation
   with the caller's authority instead.
4. **Grants are per-activation and additive.** They never widen into a called
   verb, and they never substitute for the principal.
5. **Builtin frames are transparent to identity.** Any walk of the stack for a
   principal skips them.
6. **A builtin that tests a flag it could have changed re-reads it.** The cached
   flags on `TaskPermissions` are from call setup; the live form re-reads from the
   transaction.
7. **`caller_perms()` is a report, not an authorisation.** It exists so MOO code
   can make its own decision. Kernel code must not authorise with it.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| `E_VERBNF` where the verb clearly exists | The verb has no `x` flag, so method lookup filtered it out | Set `x`, or grant `verb_call`. |
| `E_PERM` from a property read that looks public | The property's own `r` flag is clear; the object's `r` flag does not cover properties | Set the property flag, not the object flag. |
| `set_verb_code()` fails for a task holding `verb_program` | The verb *write* check also has to pass | Add `verb_write`, or own the verb, or set its `w` flag. |
| A helper works as a wizard but fails after `set_task_perms(player, grants)` | The failing operation is inside a verb the helper called; grants do not propagate | Do the granted operation in the granting activation, or re-grant. |
| Code reads an object's flags or location that it "should not" see | No check exists for those | Expected. Model any secrecy in ordinary properties. |
| A player reaches a wizard-only builtin | A `bf_<name>` override verb on `#0` intercepted it, or the check used cached rather than live flags | Look at `#0` first; then at which authority accessor the check used. |
| A wizard-owned verb is called by untrusted code and does too much | The verb runs as its owner regardless of caller | Test `caller_perms()` at the top, or drop to the caller's identity with `set_task_perms`. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/common/src/model/task_permissions.rs` | `TaskPermissions`, `CapabilityGrant`, the owner and wizard and programmer predicates. |
| `crates/db/src/api/auth.rs` | `AuthRule` and `AuthContext`: every storage-layer rule and which error each denial produces. |
| `crates/db/src/api/world_state.rs` | The call sites. Read this to learn what is checked and what is not. |
| `crates/vm/src/exec_state.rs` | Principal lookup up the stack, `set_task_perms`, `caller_perms`. |
| `crates/vm/src/activation.rs` | Where a new activation's authority is set to the verb owner. |
| `crates/kernel/src/vm/builtins/mod.rs` | The `require_*` helpers builtins should use. |
| `crates/kernel/src/vm/builtins/bf_server.rs` | `set_task_perms`, `task_perms`, `caller_perms`, and grant parsing. |

## Where the book is behind the code

`book/src/the-moo-programming-language/task-permissions-and-capability-grants.md`
is accurate on the grant model, but its grant tables omit `object_list`, which the
parser accepts and which gates object enumeration.

## Read first / read next

Read `moor/storage-and-state/world-state-model` for what each checked operation
does. Read `builtin-functions` before you write a check inside a builtin. Read
`virtual-machine` for how the authority is chosen when an activation is pushed.
