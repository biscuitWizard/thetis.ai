---
name = "Recovering from a bad change"
brief = "Diagnose whether a change broke the build, the load, or the behaviour, then reset the branch to the right commit."
when_to_use = "Use when something worked before a change and does not now: a component that will not load, a tool that returns errors, or behaviour that regressed. Use before attempting a third fix on the same problem."
tags = ["rollback", "recovery", "self-mod", "debugging", "branch"]
children = "none"
version = 2
---

Three failures look similar from the outside and want different responses.

## Tell them apart first

| Symptom | What broke | What to do |
|---|---|---|
| Compile errors in the tool result | The build | Fix and re-call; nothing is loaded yet |
| Builds clean, component will not load | The contract | Check WIT against the host; see `careful-surgery/contract-changes` |
| Loads, behaves wrongly | The logic | Read the code; reset if two fixes have failed |

The middle row is the one that misleads. A successful build followed by a
missing component is almost never a logic bug.

## Where your code lives

This conversation runs on its own git branch of the source tree, in its own
checkout. Every successful build, skill edit, and end of turn is a commit on
that branch — `branch_log` shows them, `branch_status` shows where the branch
stands against trunk. Nothing you change here touches any other conversation
until a human merges the branch to trunk.

## Resetting

`branch_log` first, then `reset_branch rev=<commit>`. Read the log rather than
assuming the previous commit is the one you want: an intervening edit may be
worth keeping.

A reset restores the whole tree to that commit **as a new commit** — history
is kept, nothing is rewritten, and the affected components are rebuilt (or
reloaded from cache) automatically. There is no per-component reset; if only
one file regressed, editing it back is lighter than resetting.

If the runtime itself is broken and keeps failing, the watchdog resets the
failing component's source to the branch's last green build on its own.

## The two-attempt rule

If two attempts have not fixed a regression, stop fixing and reset. The
third attempt is usually built on a wrong model of the problem, and each one
makes the diff harder to revert cleanly.

After resetting, reproduce the original problem before changing anything.
A bug you cannot reproduce is a bug you cannot confirm you fixed.

## What a reset does not undo

The branch covers the source tree: components, skills, the WIT contract, the
kernel, this branch's thetis.toml. It does not cover:

- Database state, including session history and remembered notes
- Files in the shared `workspace/` directory
- Anything already merged to trunk (a human can reset trunk from /admin)

If a change wrote to any of those, undo it explicitly.
