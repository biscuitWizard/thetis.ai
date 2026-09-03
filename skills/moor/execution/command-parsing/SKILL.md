---
name = "The mooR command parser"
brief = "How a line a player types becomes a verb call: $do_command, word splitting, prepositions, object matching against the player's surroundings, and :huh."
when_to_use = "Use when working on command handling in the mooR server: $do_command, object matching against the player's surroundings, ambiguous or failed matches, ordinal matching such as \"second lamp\", or the :huh fallback. Not for the scheduler, the VM, permission rules, or writing a builtin. Not for telnet/web line handling, the Torchship database, or Thetis's internals."
universal = false
tags = ["moor", "command parser", "do_command", "dobj", "iobj", "prepstr", "argstr", "preposition tables", "object matching", "aliases", "huh", "parse_command", "find_command_verb", "dispatch_command_verb", "ambiguous match", "complex_match", "verb argument specs"]
version = 2
---

# The mooR command parser

The parser turns one line of text into one verb call. It runs inside the task
that the line created, in that task's transaction, using that task's permissions.
It is not a separate stage before the task starts.

Read `task-scheduler` for the task this happens inside.

## The whole pipeline

A line arrives from a host as a command task. Everything below happens in one
task and, unless a suspension intervenes, one transaction.

| Step | What happens |
|---|---|
| 1 | Is there a `$do_command` verb on `#0`? If yes, call it with the line's words as `args` and the raw line as `argstr`. `this` and `caller` are the handler object, normally `#0`. |
| 2 | If `$do_command` returns a true value, the task is done. If it returns a false value, the same task rewrites itself to a parsed command and continues **in the same transaction**. |
| 3 | Read the player's location. |
| 4 | Rewrite a leading punctuation alias: `"` becomes `say `, `:` becomes `emote `, `;` becomes `eval `. |
| 5 | Split into words. The first word is the verb; the rest is `argstr`. |
| 6 | Find the earliest word that is a preposition. Words before it are the direct object string; words after it are the indirect object string. With no preposition, everything after the verb is the direct object string. |
| 7 | Match each object string to an object. |
| 8 | Search four targets in order for a verb whose name and argument spec fit. |
| 9 | If none fits, try `:huh` on the player's location. If the location is nothing, or `:huh` is absent, the command fails to match. |
| 10 | Push the activation with the command environment bound. |

Step 4 is worth reading twice. The punctuation aliases are applied **inside** the
built-in parser, which runs only after `$do_command` declines. A `$do_command` verb
therefore sees `;2+2`, not `eval 2+2`.

Word splitting honours double quotes and backslash escapes. A quoted run is one
word; a backslash makes the next character literal.

## Object matching

The default matcher searches the player's inventory first, then the contents of
the player's location, then the location and the player themselves, in that order.

Before searching it handles three special cases:

- A string beginning with `#` that parses as an object reference resolves
  directly, with no search and no validity check.
- `me` resolves to the player.
- `here` resolves to the player's location.

The name match runs over each candidate's name and its aliases. mooR's default
matcher, the one commands use, is the "complex" matcher: it classifies each
candidate as an exact, prefix, or substring match, in that precedence, and can
also fall back to an edit-distance fuzzy tier. It understands ordinals, so
"second lamp" or "3rd book" selects among equal-tier matches.

The outcomes are the LambdaMOO ones:

| Outcome | Value |
|---|---|
| Empty object string | No object at all; the argument spec sees "none". |
| One match | That object. |
| Several at the same tier, no ordinal | The ambiguous-match sentinel, plus the candidate list. |
| No match | The failed-match sentinel. |

A simpler exact-and-prefix-only matcher also exists and is what the older
LambdaMOO algorithm did. It is retained for tests and for callers that want it;
commands do not use it.

Ambiguity is not an error at parse time. The sentinel flows into the verb
lookup, where it will fail to match a `this` spec, and the core is expected to
report it.

## Prepositions

The preposition set is fixed. It is the LambdaMOO 1.8.1 list — with/using, at/to,
in front of, in/inside/into, on top of/on/onto/upon, out of/from inside/from,
over, through, under/underneath/beneath, behind, beside, for/about, is, as,
off/off of — plus one mooR addition, named/called/known as.

Each preposition has a canonical multi-form string used in verb definitions and a
single-word form used where space separation is required, such as objdef files.
A preposition can also be named by its numeric id. Lookup falls back to an
edit-distance match, so a near-miss spelling in a verb definition still resolves.

The set is not extensible from the database. Adding one is a Rust change, and it
changes stored verb argument specs, so treat it as a compatibility change.

## Finding the verb

Four targets are tried in order: the player, the player's location, the direct
object, the indirect object. For each, the parser builds an argument spec from
the parsed command **relative to that target**:

- If the object is the target itself, the spec position is `this`.
- If the object is nothing, the spec position is `none`.
- Otherwise it is `any`.

The verb name must match — verb names may use `*` as a wildcard — and the
argument spec and preposition spec must fit. The first target with a fit wins.

Command lookup differs from ordinary method dispatch in one way that changes the
failure mode. Method dispatch filters on the `x` flag during resolution, so a verb
without `x` is simply not found. Command lookup does not filter; it resolves the
verb and then applies the authorisation rule, which still needs `x`, ownership,
the wizard bit, or a `verb_call` grant. A command verb without `x` therefore
produces a permission error and stops the search, rather than falling through to
the next target.

The activation runs as the verb's owner. A permission failure at any step of
parsing or lookup becomes a command-level permission error, not a MOO exception.

## What the verb receives

| Variable | Value |
|---|---|
| `player` | The player who typed the line |
| `this` | The target the verb was found on |
| `caller` | The player |
| `verb` | The first word of the line, after any punctuation alias |
| `argstr` | Everything after the first word |
| `args` | The words of `argstr` |
| `dobjstr`, `dobj` | The direct object string and the object it matched |
| `prepstr`, `prep` | The preposition as typed, and its spec |
| `iobjstr`, `iobj` | The indirect object string and the object it matched |

## Where a core can and cannot intervene

**Can:**

- **`$do_command` on `#0`.** The whole line, before any parsing. Returning a true
  value replaces the built-in parser entirely. This is the intended extension
  point.
- **`:huh` on the player's location.** Called with the parsed command when no
  verb matched.
- **`parse_command()`.** Runs the parser over an arbitrary string against an
  explicitly supplied environment list, and returns the parts as a map. The
  environment is a list of objects, or of object-plus-names entries, so a core can
  match against a set it chose rather than the player's surroundings. Optional
  arguments turn on complex matching and set the fuzzy threshold.
- **`find_command_verb()`.** Takes a parsed-command map and an environment list
  and reports which verbs would match. Together with `parse_command()` this
  reproduces the built-in parser in MOO code, and lets a core retry with different
  direct and indirect object choices to resolve ambiguity.
- **`dispatch_command_verb()`.** Wizard-only, or with a matching builtin-call
  grant. Executes a command verb with a full command environment. It sets an
  explicit caller-permissions override so the dispatched verb sees the player as
  its caller, not the wizard that dispatched it.
- **`$do_out_of_band_command`.** A separate entry point. The host decides which
  lines are out-of-band, and those become a verb task on that verb instead of a
  command task. They never reach the parser.

**Cannot:**

- Change the preposition set.
- Change the word-splitting rules, the punctuation aliases, or the ordering of the
  four verb targets.
- Change the search order of the matching environment.
- Change the matcher's tier precedence or its fuzzy threshold for the built-in
  path. Only the `parse_command()` builtin exposes the threshold.

Line handling before the command reaches the server — the flush command, the
`.program` mode, holding input for a pending `read()`, and the out-of-band prefix
— belongs to the host. See `moor/services/hosts-and-sessions`.

## Invariants

1. **Parsing happens inside the task's transaction and under the task's
   permissions.** Object matching reads names and contents through the world
   state, so a matching failure can be a permission failure.
2. **`$do_command` returning false does not restart the task.** The same task, the
   same task id, and the same transaction continue into the built-in parser.
3. **`x` is checked differently for a command verb.** Method dispatch filters on
   it during resolution; command dispatch resolves first and then authorises, so a
   missing `x` denies rather than skips.
4. **The matcher never raises.** Ambiguity and failure are sentinel object values
   that flow into verb lookup. Do not convert them into errors in the parser.
5. **The parser is one implementation behind one trait.** `CommandParser` and
   `ObjectNameMatcher` are the seams. Add behaviour by implementing them, not by
   special-casing inside the default parser.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A command silently does nothing | `$do_command` returned a true value and did no work | Check `$do_command` before blaming the parser. |
| `;` or `"` reaches `$do_command` unexpanded | The aliases are applied after `$do_command` declines | Expected. Handle the raw punctuation in `$do_command` if you intercept it. |
| A command that should match reports no match | The verb's argument spec does not fit, or the preposition in the definition is not in the table | Compare the verb's spec against what the four-target rule builds. |
| A verb never matches on the direct object | The direct object string matched ambiguously or not at all, so the spec is not `this` | Look at `dobj` in the failing case. |
| An object in the room does not match by name | It has no names, is invalid, or the task lacks permission to read it | Names come from the world state under the task's permissions. |
| A permission error instead of a no-match | Object matching hit a denied read while walking the surroundings | Expected. The parser maps a denied read to a command permission error. |

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/common/src/matching/mod.rs` | `ParsedCommand`, `MatchResult`, and the `CommandParser`, `ObjectNameMatcher`, `MatchEnvironment` traits. |
| `crates/common/src/matching/command_parse.rs` | The default parser: aliases, word splitting, preposition seeking. |
| `crates/common/src/matching/prepositions.rs` | The preposition table and its lookups. |
| `crates/common/src/matching/complex_match.rs` | Tiered and fuzzy matching, and ordinal parsing. |
| `crates/common/src/matching/complex_object_matcher.rs` | The matcher commands use. |
| `crates/common/src/matching/match_env.rs` | The simpler exact-and-prefix matcher. |
| `crates/common/src/matching/ws_match_env.rs` | The world state as a matching environment: names, surroundings, location. |
| `crates/common/src/model/world_state.rs` | `command_verb_argspec` and the verb lookup request types. |
| `crates/kernel/src/tasks/task.rs` | `$do_command`, the parse phase, the four-target verb search, `:huh`. |
| `crates/kernel/src/vm/builtins/bf_objects.rs` | `parse_command`, `find_command_verb`, `dispatch_command_verb`. |
| `crates/common/src/util/mod.rs` | Word splitting with quotes and escapes. |

## Read first / read next

Read `moor/storage-and-state/world-state-model` for how names, aliases, and
contents are stored. Read `permissions-and-security` for why a match can fail with
a permission error. Read `moor/services/hosts-and-sessions` for what the host does
to a line before it becomes a command task.
