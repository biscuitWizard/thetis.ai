---
name = "Careful self-modification"
brief = "Work in small reversible steps when changing your own loop, gateways or tools."
when_to_use = "Use whenever the target of a change is Thetis itself: the agent loop, a gateway, a tool, the WIT contract, or the orchestrator crate. Not for ordinary file edits in a user's project."
universal = true
tags = ["self-mod", "safety", "devkit", "rollback", "tool-group:selfmod", "tool-group:branch"]
children = "auto"
version = 2
---
# Careful self-modification

When you change your own loop, a gateway, or a tool, treat it as surgery on a
running patient. The patient is you, and the anaesthetic is the revision system.

## The loop

1. **Read before writing.** `read_code` the file you are about to change. The
   version in your head may be several revisions stale.
2. **Change one thing.** A patch that touches one concern can be reverted
   cleanly. A patch that touches four cannot.
3. **Read the compile verdict.** It comes back in the same tool result. A build
   failure means the old component is still loaded, so nothing is broken yet —
   fix it and call again.
4. **Verify behaviour, not compilation.** A green build says the types line up,
   not that the thing works.
5. **Roll back rather than pile on.** If two attempts have not fixed a
   regression, `reset_branch` and start again from a green commit.

## What makes a change risky

Risk is not proportional to diff size. It comes from how much depends on the
thing you are touching:

- **The WIT contract** is the highest-risk surface. Changing a record breaks
  every guest at instantiation, not at compile time, so the failure appears
  after a successful build.
- **Your own loop** is next: a bad edit can stop you from making further edits.
  Keep a known-good revision in mind before you start.
- **A single tool** is nearly free to break. It fails in isolation and the rest
  of the system keeps running.

Match caution to that ordering rather than to how large the edit feels.

## Nested topics

- [contract-changes](skill:careful-surgery/contract-changes) — the ordering that
  keeps guests loadable when `wit/thetis.wit` gains or loses something. Read it
  *before* editing the contract, not after a guest stops instantiating.
- [recovering-a-revision](skill:careful-surgery/recovering-a-revision) — what to
  do when a build is green but the behaviour regressed, or a component will no
  longer load at all.
