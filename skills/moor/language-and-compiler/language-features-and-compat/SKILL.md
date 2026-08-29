---
name = "MOO language features and LambdaMOO compatibility"
brief = "Which MOO features are mooR extensions over LambdaMOO, how CompileOptions and FeaturesConfig select them, and what the engine guarantees an imported core."
when_to_use = "Use when deciding whether a MOO language feature is standard LambdaMOO or a mooR extension, when adding a feature switch, when a verb fails to compile with a DisabledFeature or UnknownBuiltinFunction error, when a LambdaMOO or ToastStunt core will not import, or when choosing the features section of a daemon config or the moorc flags. Not for how to write MOO verbs in a game; torchship/torchship-programming/moor-book teaches MOO as an authoring language, including syntax, builtins and verb-writing gotchas. This skill is about how the engine defines the language, not how to use it. Not for the compiler stages, which are compiler-pipeline. Not for the value types themselves, which are value-model. Not for textdump file parsing, which is moor/content-pipeline/textdump-compat. Not for the Torchship database. Not for Thetis internals."
universal = false
tags = ["moor", "moo", "lambdamoo", "toaststunt", "compatibility", "extensions", "compileoptions", "featuresconfig", "feature flags", "disabled feature", "legacy type constants", "import", "core", "moorc"]
version = 1
---

# MOO language features and LambdaMOO compatibility

mooR runs LambdaMOO 1.8.x code and adds to the language. This skill says which parts
are additions, how each one is turned on and off, and what an operator or an importer
can rely on.

**This is the engine's view.** For MOO as a language you write verbs in — syntax,
which builtin to call, what surprises an author — read
`torchship/torchship-programming/moor-book`. Here the question is what the compiler
accepts, what the switch is called, and what breaks in a core when it changes.

## Two different switch sets

This is the first thing to get right, and it is easy to get wrong.

| | `CompileOptions` | `FeaturesConfig` |
|---|---|---|
| Defined in | `crates/compiler/src/compile_options.rs` | `crates/vm/src/config.rs` |
| Scope | One call to `compile()` | The whole server process |
| Decides | Which syntax the compiler accepts | Server behaviour, including which `CompileOptions` the server uses |
| Set by | The caller, per compile | The `features:` section of the daemon config file, then CLI flags |

`FeaturesConfig::compile_options()` derives a `CompileOptions` from the server's
configuration. The daemon compiles every verb through that. So in a running server,
`FeaturesConfig` is the authority and `CompileOptions` is its projection.

**The two defaults disagree, on purpose.** `CompileOptions::default()` enables custom
errors; `FeaturesConfig::default()` disables them. The compiler crate's default is
"accept everything mooR can express", which is what tests and tools want. The
server's default is "safe for an existing core". Never assume one from the other.
Read both `Default` implementations before you rely on either.

## The compile options

Only these six things change what the compiler accepts.

| Option | Effect when off | Failure the author sees |
|---|---|---|
| `flyweight_type` | `< delegate, .slot = v >` is rejected | `CompileError::DisabledFeature`, "Flyweights" |
| `bool_type` | `true` and `false` are not literals | `DisabledFeature`, "Booleans" |
| `symbol_type` | `'name` is not a literal | `DisabledFeature`, "Symbols" |
| `custom_errors` | Only the standard `E_` codes are accepted | `DisabledFeature`, "CustomErrors" |
| `call_unsupported_builtins` | An unknown function name is an error rather than a rewrite | `CompileError::UnknownBuiltinFunction` |
| `legacy_type_constants` | `INT`, `OBJ`, `STR` and the rest are ordinary identifiers | Nothing; they become variables |

Every gate except one lives in `frontend/lower.rs`. `call_unsupported_builtins` is
read in `backend/expr_codegen.rs`, because it can only be decided once the builtin
lookup fails. If you add a feature gate, put it in one of those two places.

### The two import options

`call_unsupported_builtins` and `legacy_type_constants` are not user-facing features.
They exist so foreign source can be read.

- **`call_unsupported_builtins`** rewrites a call to an unknown function `foo(...)`
  into `call_function('foo, ...)`, with a warning. A LambdaMOO or ToastStunt core
  calls builtins mooR does not have; without this the whole import fails on the first
  one. The rewritten call still fails at run time, but at the call site and only if
  reached. The textdump importer and the objdef loader both turn it on. The daemon
  never does.
- **`legacy_type_constants`** makes the short type names parse as type literals
  instead of variables. The textdump importer turns it on for every verb it compiles.
  `moorc --legacy-type-constants true` turns it on for an objdef migration; the
  output uses the new `TYPE_*` form, so the migration is one-way and one-time.

Note the asymmetry: `TYPE_INT` and the other prefixed forms are recognised by the
lexer unconditionally and are always type literals. The short forms are lexed as
identifiers and reinterpreted by lowering only when the option is on. So a core that
uses `STR` as a variable name keeps working by default, which is the point.

## The runtime-only feature switches

These are in `FeaturesConfig` and never reach the compiler. Get the current list and
defaults from `crates/vm/src/config.rs` and from the daemon's `--help`.

| Switch | What it changes |
|---|---|
| `type_dispatch` | Whether `"text":verb()` dispatches to `$string:verb("text")` |
| `use_boolean_returns` | Whether comparison operators and truth-returning builtins yield a boolean or LambdaMOO's integer `1`/`0` |
| `use_symbols_in_builtins` | Whether builtins that name properties and verbs return symbols instead of strings |
| `rich_notify` | Whether `notify()` accepts a non-string value |
| `persistent_tasks` | Whether suspended and forked tasks survive a restart |
| `use_uuobjids` | Whether `create()` allocates UUID object ids |
| `anonymous_objects` | Whether anonymous objects may be created |
| `enable_eventlog` | Whether events are persisted and history is available |

`use_boolean_returns` and `use_symbols_in_builtins` are off by default and are marked
in the source as compatibility risks. Both change the *type* of values that existing
core code compares against. A core that writes `if (x == 1)` after a comparison
breaks when comparisons start returning booleans.

Two switches are deprecated and ignored: `lexical_scopes` and `list_comprehensions`.
Both features are always on. `normalize_deprecated_flags` forces them back to true
and logs a warning. Do not add code that reads them.

## Compile options gate syntax, not capability

This distinction catches people.

Turning `flyweight_type` off stops the compiler accepting flyweight syntax. It does
not remove flyweights from the running system. The `MakeFlyweight` opcode has no
runtime gate, so a program compiled while the feature was on still builds flyweights
after it is turned off. What does change is that the flyweight builtins refuse with
`E_PERM` and the message "Flyweights not enabled".

Runtime gating is per builtin and is not uniform. If you need a feature to be truly
absent at run time, you must add the check where the value is produced, not only in
the compiler. Ask yourself which one you actually want before you add a switch.

## What is a mooR extension

The features below are additions to LambdaMOO 1.8.x. Confirm the switch name and its
default in `crates/vm/src/config.rs` before you quote one.

**Always on, no switch:**

- Lexical scoping with `let`, `const`, `global`, and `begin`/`end` blocks. LambdaMOO
  had only verb-global variables bound at first assignment. That rule still applies
  to an undeclared assignment, so old code is unaffected.
- List and range comprehensions.
- `return` usable as an expression, for short-circuit returns.
- Lambdas and named inner functions, with capture by value. LambdaMOO had none,
  despite the name.
- The map type, which came from Stunt and ToastStunt rather than LambdaMOO.
- Binary values, with a `b"..."` literal.
- Error messages attached to error values.
- Type constants as literals with a `TYPE_` prefix, replacing LambdaMOO's
  pre-populated variables.
- 64-bit integers, and UTF-8 strings.

**Switchable:**

- Flyweights, booleans, symbols and custom errors, through the four compile options.
- Primitive-type verb dispatch, boolean returns, symbol-returning builtins, UUID and
  anonymous object ids, rich `notify`, through the runtime switches.

**Different in kind, not a language feature:** multiversion concurrency with
serializable isolation replaces LambdaMOO's single global lock. This changes what a
verb can assume about the world between two statements far more than any syntax does.
Read `moor/storage-and-state/transactions`.

One more difference visible to the compiler: inside a lambda body, an undeclared
identifier binds in the enclosing lexical scope rather than in the verb-global scope.
Outside a lambda it binds globally, as in LambdaMOO.

## Where the switches are set

| Setting | Where |
|---|---|
| The `features:` section of the daemon config | A YAML file, for example `moor-dev.yaml` |
| Per-run overrides | Daemon CLI flags, defined in `crates/daemon/src/feature_args.rs` |
| Import and migration | `tools/moorc/src/main.rs`, which takes the same feature flags plus `--legacy-type-constants` |
| Ad-hoc evaluation | The MCP host's eval tool, which exposes `legacy_type_constants` |

CLI flags are merged over the config file. Deprecated flags are normalised after the
merge. There is no per-object or per-verb feature selection; the setting is
process-wide.

**The configuration is not stored in the database.** A world can be restarted with a
different feature set, and verbs compiled under the old set remain compiled. Nothing
recompiles them. So turning a compile option off does not remove the feature from
code already in the database; it only stops new code from using it.

## What the engine guarantees, and what it does not

Guaranteed:

- LambdaMOO 1.8.x source compiles, with the default server feature set.
- The standard error codes keep their integer values, so `tonum()` on them still
  works.
- The standard global variables exist in every verb frame.
- Undeclared assignment still creates a verb-global variable.
- A textdump from LambdaMOO or ToastStunt can be imported, with unknown builtins
  rewritten and legacy type constants recognised.

Not guaranteed:

- That every builtin a foreign core calls exists. Check the builtin status document
  under `book/src/the-moo-programming-language/built-in-functions/` and the builtin
  table in `crates/common/src/builtins.rs`.
- That a verb's source text survives. It does not; you get the decompiled form. See
  `compiler-pipeline`.
- That traceback line numbers match a source file. They match the unparsed form.
- That behaviour under concurrency matches a single-lock server.
- That a core written against mooR extensions runs on LambdaMOO. Compatibility runs
  one way.

## Adding a language feature

1. **Decide whether it needs a switch.** It does if a LambdaMOO-era core could break,
   either because the syntax was previously legal as something else, or because a
   value's type changes.
2. **Add the field** to `CompileOptions`, to `FeaturesConfig`, to
   `FeaturesConfig::compile_options()`, and to `FeatureArgs`. Missing the last one
   means the switch is unreachable from the command line.
3. **Gate it in lowering**, with `CompileError::DisabledFeature` and a name the author
   will recognise.
4. **Decide whether it also needs a runtime gate.** See the section above.
5. **Add the unparser case and the decompiler case.** Without both, a verb that uses
   the feature cannot be read back. See `compiler-pipeline`.
6. **Add the stored form** if it introduces a new opcode or a new value kind. See
   `program-and-opcodes` and `moor/services/wire-schema`.
7. **Add Moot tests.** See `moor/working-in-the-repo/testing`.
8. **Update the book.** `book/src/the-moo-programming-language/extensions.md` is the
   list users read.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| `DisabledFeature` on a verb that used to compile | The server's `features:` section changed | Check the config and the CLI flags, not the code. |
| `UnknownBuiltinFunction` during an import | `call_unsupported_builtins` was not on for that path | The textdump importer and the objdef loader turn it on. A direct `compile()` does not. |
| A legacy core fails on `STR = "x"` | `legacy_type_constants` is on, so `STR` is a literal and cannot be assigned | Only turn it on for import and migration. `CompileError::InvalidTypeLiteralAssignment` names it. |
| Core code that tested `== 1` starts failing | `use_boolean_returns` was turned on | It is off by default for this reason. Turn it off, or fix the core. |
| A verb builds a flyweight although flyweights are off | The verb was compiled while they were on; the opcode has no runtime gate | Recompile the verb, or add the runtime check. |
| Deprecated flag has no effect | `lexical_scopes` and `list_comprehensions` are ignored | Remove them from the config. The warning in the log says so. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/compiler/src/compile_options.rs` | The six compile options and their defaults |
| `crates/vm/src/config.rs` | `FeaturesConfig`, its defaults, deprecation normalisation, and the projection to `CompileOptions` |
| `crates/daemon/src/feature_args.rs` | The CLI flags and how they merge over the config file |
| `crates/daemon/src/args.rs` | Config file loading and merge order |
| `crates/compiler/src/frontend/lower.rs` | Every `DisabledFeature` gate, and the legacy type constant path |
| `crates/compiler/src/backend/expr_codegen.rs` | The unknown-builtin rewrite |
| `crates/textdump/src/load_textdump.rs` | Import-time option overrides for textdumps |
| `crates/objdef/src/load.rs` | Import-time option overrides for objdef directories |
| `tools/moorc/src/main.rs` | The offline compiler and migration tool |
| `crates/common/src/builtins.rs` | The builtin table |

## Where the book is behind the code

`book/src/the-moo-programming-language/extensions.md` is the readable summary. It is
a narrative, not a switch list: it names the CLI flag for some features and not for
others, and it does not say that custom errors are off by default in the server even
though it presents them as an extension. Take the switch names and defaults from
`crates/vm/src/config.rs` and from the daemon's `--help`, never from the book.

`book/src/the-database/moo-value-types.md` omits the boolean type from its list of
value kinds, although the boolean literal is on by default. See `value-model`.

## Read first / read next

Read `compiler-pipeline` first, because every gate lives in one stage of it.

After this, read `moor/content-pipeline/textdump-compat` for importing a LambdaMOO
database, `moor/content-pipeline/objdef-format` for the source-directory format,
`moor/execution/builtin-functions` for which builtins exist and what changing one
costs, and `moor/working-in-the-repo/testing` for how to assert a language change.
