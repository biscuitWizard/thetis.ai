---
name = "The MOO value model"
brief = "How MOO values work in mooR: the 16-byte immutable Var, structural sharing, symbols, flyweights, errors as values, and what each operation costs."
when_to_use = "Use when working with a MOO value, a comparison behaves unexpectedly, a builtin needs to raise or return an error, or you are reasoning about the memory cost of a data structure. Not for how the VM unwinds an error (read moor/execution/virtual-machine), not for storage (read moor/storage-and-state/world-state-model), and not for the program format (read program-and-opcodes)."
universal = false
tags = ["moor", "moo", "var", "variant", "value types", "immutable", "symbol", "flyweight", "lambda", "binary", "obj", "str", "errorcode", "error", "e_type", "list", "map", "string", "case insensitive", "memory", "performance", "moor-var", "crates/var"]
version = 2
---

# The MOO value model

`crates/var` defines every value a MOO program can hold, and every value the
database can store. `Var` is the single runtime type; `Variant` is the borrowed view
that lets you match on which kind it is.

## The three rules

1. **Every value is immutable.** No operation modifies a value in place. `list[1] = x`
   produces a new list. A verb that "appends to a property" reads the value, builds a
   new one, and writes it back.
2. **Every value is cheap to copy.** `Var` is 16 bytes. Cloning it copies those 16
   bytes and, for the reference-counted kinds, bumps one refcount.
3. **Large values share structure.** A "copy" of a list with one element changed
   shares nearly all of the original's memory.

Immutability is not a style choice here. It is what makes multi-version concurrency
work: two transactions can hold the same value at the same time with no locking and
no defensive copying, and a value written into a transaction cannot be changed under
another reader. Read `moor/storage-and-state/transactions` for what that buys.

## How a Var is laid out

`Var` is `repr(C)`: a one-byte type tag, seven metadata bytes, and an eight-byte
payload. A compile-time assertion pins the size to two machine words. The tag's high
bit is a "complex" flag: tags below it hold their whole value inline, and tags at or
above it hold a pointer to reference-counted storage. That one bit is what makes
clone and drop a single branch.

Simple, inline: none, boolean, integer, float, object reference, symbol, empty
string, empty list. Complex, refcounted: string, list, map, error, flyweight,
binary, lambda.

Three details in the metadata bytes matter:

- **Lists, maps and strings cache their length** in the metadata, with a sentinel for
  overflow. `length()` on a large list does not chase the pointer.
- **Strings cache a pure-ASCII flag**, which is what lets case-insensitive comparison
  take a fast path.
- **One byte holds an operation hint**: how this value was produced, such as "a list
  append" or "a map insert". The hint is read by the transaction conflict resolver so
  that two tasks appending to the same list can sometimes be merged rather than
  aborted. Hints are cleared before a value is committed. Do not attach meaning to a
  hint outside that use. See `moor/storage-and-state/transactions`.

## What each kind is, and what it costs

| Kind | Backing | Cost model |
|---|---|---|
| Integer, float, boolean, object, none | Inline in the payload | Free to copy. No allocation. |
| Symbol | Two 32-bit ids, inline | Free to copy. Equality is an integer compare. |
| String | `arcstr::ArcStr` | Copy is a refcount. Concatenation allocates a new string and copies both. |
| List | `imbl::Vector` | Copy is a refcount. Index, push and set are logarithmic and share structure. |
| Map | `imbl::OrdMap`, ordered by key | Copy is a refcount. Lookup and insert are logarithmic. Iteration is in key order. |
| Binary | `byteview::ByteView` | Copy is cheap; `ByteView` handles the sharing. |
| Flyweight | Boxed delegate, shared sorted slot vector, contents list | Copy is a refcount. Slot lookup is a binary search. |
| Lambda | `triomphe::Arc` over params, body program and captured environment | Copy is a refcount. Creating one copies the captured values. |
| Error | Inline code, or a boxed record when it carries a message or value | 16 bytes. A bare code allocates nothing. |

The persistent collections are the reason the cost model works. `imbl::Vector` and
`imbl::OrdMap` already share structure internally, so `List` and `Map` box them
rather than wrapping them in another `Arc`. Do not add a second layer of reference
counting around them.

**The trap to warn authors about:** building a long list by repeated append is not
free in aggregate. Each append is cheap, but each one produces a new value, and the
old one stays alive as long as anything holds it. The transaction conflict hints
exist because this pattern is common, not because it is free.

## Equality is case-insensitive, and that has consequences

MOO string comparison ignores case, and so does symbol comparison. `"A" == "a"` is
true. `Var`'s `PartialEq` implements this, with an ASCII fast path.

Two places must not use it:

- **Literal pooling in the compiler** uses `eq_case_sensitive`. Pooling on `==` would
  make the program's literal table replace `"Foo"` with an earlier `"foo"`, and the
  verb would print the wrong string. See `compiler-pipeline`.
- **Anything that must preserve the author's spelling** must go through
  `eq_case_sensitive` or compare the underlying string directly.

Several list and map operations take an explicit `case_sensitive` argument for the
same reason. When you write a builtin that searches, decide which one you want and
say so; do not accept the default without thinking.

**Map keys are restricted.** A key must be a scalar or a string. Lists, maps,
flyweights and other containers are rejected with `E_TYPE`. This restriction is
inherited from ToastStunt for compatibility, not forced by the implementation.

**Truthiness follows LambdaMOO.** Zero, an empty string, an empty list, an empty
map, an empty binary and a flyweight with no contents are false. An object reference
is always false. An error value is always false. A symbol and a lambda are always
true.

## Symbols

A symbol is an interned string with two 32-bit ids: a *compare id* shared by every
case variant of the same text, and a *repr id* naming one exact spelling. So two
symbols compare equal without touching memory, and each still knows the case it was
written with.

Symbols exist because the engine constantly compares short names: verb names,
property names, variable names, flyweight slot names, builtin names. Interning turns
those comparisons into integer compares and removes the allocation.

Three consequences you must not forget:

1. **The interner is process-global and append-only.** Nothing is ever removed. A
   symbol created from untrusted, unbounded input is a permanent leak. Never intern
   arbitrary user text in a loop.
2. **Symbol ids are process-local.** They mean nothing in another process or after a
   restart. Symbols serialise as their text, both in FlatBuffers and in serde. Never
   persist or transmit an id.
3. **Interning is fast but not free.** There is a small thread-local cache in front of
   a lock-free global map. Prefer holding a `Symbol` you already have over making it
   again from a string.

Symbols are also a language-visible type, written `'name`. The `symbol_type` compile
option controls the literal. See `language-features-and-compat`.

There is one more use of the interner: a string `Var` can be represented by a
symbol's interned text rather than its own allocation. This is invisible to MOO, but
it means "is this a string" must be asked with the provided accessor and not by
matching one tag.

## Flyweights

A flyweight is an object-shaped *value*. It has a delegate object, a set of named
slots, and a contents list. Verb calls on a flyweight dispatch to the delegate, with
`this` bound to the flyweight itself. Property reads resolve in the slots first, then
fall through to the delegate. Slots are read-only; you build a new flyweight to
change one.

The problem it solves: MOO's only aggregate with behaviour was the database object,
and a database object is expensive. It occupies an object number, it participates in
the object graph and in transactions, and it must be recycled. Programs that want
many small structured things with methods — document nodes, events, UI elements,
short-lived entities — could not have them.

A flyweight is stored inside a property, a variable, a list or a map, like any other
value. It has no object number, no location in the object tree, and no verbs of its
own. It is freed by reference counting when nothing holds it.

Two names are reserved: a slot may not be called `delegate` or `slots`, because both
are how the flyweight itself is inspected. Lowering rejects them with
`CompileError::BadSlotName`, and so does the objdef literal parser.

Slots are canonicalised on construction: sorted by symbol, with duplicates collapsed
to the last value. So two flyweights built from the same pairs in different orders
are equal, and lookup is a binary search.

The book chapter `book/src/the-database/flyweights.md` is a good introduction. It
says flyweights are "automatically garbage collected". They are reference counted;
there is no tracing collector for them. That matters if you ever build a cycle.

## Errors are values

An error is an ordinary MOO value. It can be returned, stored in a property, put in a
list and compared. This is the largest difference from a conventional language, and
it is inherited from LambdaMOO.

An `Error` is an `ErrorCode` plus, optionally, a message string and an attached
value. The bare-code form allocates nothing; the rich form boxes a record. The
standard codes are an enum in `crates/var/src/error.rs`; do not write the list
anywhere else, and get the current set from that enum. `ErrCustom(Symbol)` is the
mooR extension: any `E_`-prefixed identifier becomes an error, with no integer
mapping. `to_int` returns nothing for a custom error, which is why `tonum()` on one
fails.

Two equality rules will surprise you:

- **Equality ignores the message.** Two errors are equal when their code and their
  attached value are equal. `E_PERM("a")` equals `E_PERM("b")`.
- **Hashing uses only the code.** So an error used as a map key collapses its
  variants together. This is a known wart, marked as such in the source.

### Raising is a separate decision from being

Whether an error value *raises* is decided by the running verb, not by the value. The
VM's `push_error` sets the error as the current value and then raises it only if the
innermost non-builtin verb frame has the `d` (debug) flag set. With `d` clear, the
error is simply the result, and execution continues. This is LambdaMOO's rule and it
is why old cores test return values for errors instead of using `try`.

Consequences for engine work:

- A builtin returns an error through the VM's error path, not by returning a plain
  error value, so the `d` flag is honoured.
- Never assume an error stops a verb. Read `moor/execution/virtual-machine` for the
  unwinding path and for `try`/`except` handling.
- An error with a message is much more useful to an author than a bare code. Most
  builtins and value operations attach one. Follow that when you add one.

## Object references

`Obj` is a single packed 64-bit value whose top two bits select the kind: a
traditional 32-bit signed object number, a 62-bit UUID-based id, or an anonymous
object id. The distinguished values `#0`, `#-1`, `#-2` and `#-3` are the system
object, nothing, ambiguous match and failed match.

The packing is why an object reference costs nothing to copy and why all three kinds
fit one type. Which kind `create()` produces is a runtime feature switch. See
`language-features-and-compat` and `moor/storage-and-state/object-lifecycle-and-gc`.

## Lambdas capture by value

A lambda holds its parameter specification, a fully compiled body `Program`, and a
captured environment copied at the moment of creation. Capture is by value, and the
compiler rejects assignment to a captured variable with
`CompileError::AssignmentToCapturedVariable`.

A self-recursive lambda would form a reference cycle if it held itself. It does not:
`for_self_reference` makes a distinct copy for the self slot. There is no cycle
collector, so a cycle would leak. Keep that in mind if you add another way for a
value to hold itself.

## Encoding and comparison utilities

| Module | Purpose |
|---|---|
| `crates/var/src/cbor.rs` | CBOR encoding of a `Var`, with its own version number. Used where a self-describing format is wanted. |
| `crates/var/src/encode.rs` | The `ByteSized` trait and the encode/decode error types. |
| `crates/var/src/diff.rs` | Structural diff of two values, and a three-way merge, both bounded by depth, change count and comparison budget. Exposed to MOO and used for conflict presentation. |
| `crates/schema/src/convert_var.rs` | The FlatBuffer form, which is what the database and the wire protocol use. See `moor/services/wire-schema`. |

Not every value can cross every boundary. A lambda with a captured environment cannot
be turned into a literal: `toliteral()` refuses it with `E_INVARG`. The objdef
exporter has its own literal writer that can emit captured environments, and it is
the only one whose output the objdef literal parser can read back. See the hazards
below.

## Invariants

1. **`Var` stays two machine words.** A compile-time assertion enforces it. Anything
   bigger goes behind the complex-tag pointer.
2. **Values are never mutated in place.** Every operation that looks like mutation
   returns a new `Var`.
3. **The symbol interner never shrinks.** Interning is permanent for the life of the
   process.
4. **Symbol ids never leave the process.** Serialisation writes the text.
5. **String and symbol equality ignore case.** Any code path that must not must say
   so explicitly.
6. **A map key is a scalar or a string.** Enforced on insert with `E_TYPE`.
7. **A flyweight slot is never named `delegate` or `slots`.** Enforced at compile
   time in both literal parsers.
8. **A new value kind needs a tag, a literal form, an unparser case, a FlatBuffer
   encoding and a CBOR case.** Missing any one of them produces a value that exists
   at run time and cannot be stored, shown, or sent.

## Hazards

| Symptom | Cause | Action |
|---|---|---|
| A verb prints a string with the wrong capitalisation | Case-insensitive equality collapsed two spellings | Find the comparison. Use `eq_case_sensitive` where spelling is meaningful. |
| Memory grows without bound in a long-running world | Arbitrary text is being interned as symbols | Find the `Symbol::mk` on user input. Use a `Str`. |
| Two different errors behave as one in a map | `Error` hashes on the code only | Key on the code plus whatever else you need, not on the `Error`. |
| `toliteral()` raises `E_INVARG` on a function value | The lambda has captured variables | Expected. There is no literal form for a closure in that writer. |
| A lambda property loses its parameter names after an objdef round trip | The objdef literal parser discards lambda parameter names and optional defaults, binding every parameter to the same placeholder name | Do not rely on lambda values surviving an objdef export and import. Store the source, or a verb, instead. See `moor/content-pipeline/objdef-format`. |
| A captured lambda literal fails to parse | Two literal writers exist. The plain one emits a flat capture list; the objdef one emits per-frame braces, and only that form is what the objdef parser accepts | Use the objdef writer for anything the objdef parser will read. |
| A value type is fine at run time but vanishes on restart | No FlatBuffer encoding was added | Add it in `crates/schema`. See `moor/services/wire-schema`. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/var/src/variant.rs` | `Var`, the tag layout, `Variant`, equality, truthiness, indexing |
| `crates/var/src/lib.rs` | `VarType`, `IndexMode`, the `Sequence` and `Associative` traits |
| `crates/var/src/scalar.rs` | Arithmetic and comparison on scalars |
| `crates/var/src/string.rs`, `list.rs`, `map.rs`, `binary.rs` | The container kinds |
| `crates/var/src/symbol.rs` | The global interner and `Symbol` |
| `crates/var/src/obj.rs` | `Obj` packing and the three object id kinds |
| `crates/var/src/flyweight.rs` | `Flyweight` and slot canonicalisation |
| `crates/var/src/lambda.rs` | `Lambda` and the self-reference copy |
| `crates/var/src/error.rs` | `Error`, `ErrorCode`, messages and integer mapping |
| `crates/var/src/cbor.rs`, `encode.rs`, `diff.rs` | Encoding and structural diff |

## Where the book is behind the code

`book/src/the-database/moo-value-types.md` lists the kinds of value a MOO program can
hold and omits booleans, although `TYPE_BOOL` exists and the boolean literal is on by
default. Get the authoritative list from `VarType` in `crates/var/src/lib.rs`.

`book/src/the-database/flyweights.md` says flyweights are garbage collected. They are
reference counted.

## Read first / read next

Read `moor/storage-and-state/transactions` to understand why immutability is
load-bearing rather than decorative.

After this, read `program-and-opcodes` for how values are embedded in a compiled
program, `moor/execution/builtin-functions` for how a builtin should return a value or
an error, and `moor/services/wire-schema` for how a value is encoded on disk and on
the wire.
