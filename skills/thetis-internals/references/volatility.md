# Volatility compartments

Written in ASD-STE100 Simplified Technical English.

This document says which parts of `thetis-internals` you can trust and which
parts you must confirm. The purpose is that a fact that changes quickly can
never make the rest of the document wrong.

## The three tiers

**Tier 1 — Invariant.** These change only with a deliberate redesign. Trust
them.

- The host and guest split, and that a guest has no ambient authority.
- Per-call instantiation, and therefore that a hot swap is safe.
- The append-only event log, and that the agent is stateless between turns.
- The four disclosure levels of the skill system.
- The names of the safety nets: validation gate, epoch watchdog, circuit
  breaker, revisions and snapshots.
- The design intents in the parent skill.

These live in the skill bodies. They are safe there.

**Tier 2 — Structural.** These change when someone edits the code. They are in
the skill bodies, and a body is the correct place, but confirm one before you
depend on it.

- Function names, struct names, module names.
- The list of source files and what each one owns.
- The order of steps in a turn.
- Which paths the dev kit refuses.

Confirm with: `list_code`, `read_code`, `list_path`.

**Tier 3 — Volatile.** These change without any code edit. They are **never**
written as fact in a skill body. Get them from the tool that owns them.

| Fact | Owner |
|---|---|
| A config key and its value | `read_config`, `list_config` |
| The set of config keys | `list_config` |
| Model ids | `list_config models` |
| Mode ids | `list_config modes` |
| Your current tool list | Your own prompt |
| Component tool names | The tool list, tagged `component` |
| Revision numbers | `history` |
| Files in an aspect | `list_code` |
| Skills in the corpus | `skill_search`, `skill_lint` |
| Dependencies of an aspect | `list_dependencies` |
| Open terminal sessions | `terminal_list` |
| What is not yet built | `README.md`, section "Not yet built" |

## The rule for an author

When you add to `thetis-internals`, ask which tier a sentence is in.

- Tier 1 or Tier 2: put it in the skill body.
- Tier 3: do not write the value. Write the name of the tool that gives it.

Bad, because the number is out of date the moment someone edits the file:

> The compaction threshold is 0.6 of a 200000-token window.

Good:

> Compaction starts when the context passes `context.compact_threshold` of
> `context.window_tokens`. Read both with `read_config`.

## Snapshots

`references/snapshot.md` holds a dated copy of the Tier 3 lists. It exists for
orientation, so that a first read of the system does not need ten tool calls. It
is **not** authority. Each table in it gives the command that makes it again.

If a snapshot and a tool disagree, the tool is correct. Update the snapshot or
delete it.
