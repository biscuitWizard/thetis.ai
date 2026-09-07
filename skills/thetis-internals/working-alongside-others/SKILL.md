---
name = "Working alongside other conversations"
brief = "You are not the only agent modifying Thetis right now. What is yours, what is shared, and the two habits that used to make three agents collide."
when_to_use = "Use before building, restarting, or running anything long — and whenever you are about to change the orchestrator's own source. Use it if a build seems to hang, if a command is refused for leaving your workspace, or if you are wondering where to put a scratch experiment."
universal = false
tags = ["concurrency", "isolation", "worktree", "build", "restart", "kernel", "orchestrator", "collision", "other agents", "tool-group:branch", "tool-group:shell"]
version = 1
---

# Working alongside other conversations

Several conversations run at once, each one an agent editing Thetis itself.
This is a supported way to work, not an accident — but it only holds if you
stay inside your own workspace.

## What is yours

Your conversation has its own git branch, `conv/<id>`, checked out at its own
worktree, and its own worker process. **That worktree is a complete checkout of
the project, not a fragment.** Everything you need to read, edit, build and
test is inside it. Your terminal starts there.

Your changes are committed to your branch as you go. Nothing you do reaches
trunk until a merge, which is a separate, deliberate step.

## What is shared, and therefore dangerous

- **The trunk checkout** — the directory your worktree lives *under*. It is
  what everyone else's branch is measured against.
- **The build caches**, keyed by content and guarded for the build pipeline
  only — not for cargo you run yourself.
- **The database and the artifact store.**

The rule that follows: **never `cd` out of your worktree.** A command that tries
is refused, and the message names where it would have gone. This is not
bureaucracy — when three agents ran `cd` into the shared checkout and built
there, they fought over one cargo target directory and each other's source
tree, and the builds that did not finish were left running after their turns
ended.

## Building the orchestrator: let the restart do it

The dev kit rebuilds guest aspects — the agent, gateways, tools — and hands you
the compiler's verdict inline. It cannot reach the orchestrator's own source
under `crates/` or the contract in `wit/`.

For those: **edit, then call `restart_orchestrator`.** It notices you have
changed the kernel, rebuilds it in the background, probes the new binary, and
only then restarts your worker onto it. The result arrives in this
conversation. A build that fails restarts nothing and gives you the compiler
error; a binary that will not start is rejected and you stay on the one you
have.

**Do not run `cargo build` on the orchestrator yourself.** A release build
takes longer than a tool call, so doing it by hand means backgrounding it and
polling a log — and a build detached that way outlives your turn, holds the
cargo lock, and blocks everyone else. If you find older notes telling you to
build it in a terminal, they predate this and are wrong.

Restarting to pick up your own kernel is normal and expected. It costs your
turn nothing: an interruption the system asked for is not counted against the
turn, and your turn is picked up where it left off.

## Seeing your own interface

Editing a gateway changes only your copy. The interface every browser loads is
trunk's until your work is merged — so a UI edit builds green and changes
nothing on screen, which is easy to misread as a broken build.

Your own version is at **`/preview/<your session id>/`**, served against the
real running system rather than an empty copy of it. Rebuild `gateway:web`
first, then reload.

## Long-running things

- Prefer a foreground command with a generous timeout; you get the output and
  the system can clean it up.
- If something must outlive a single call, it still must not outlive your
  turn. `setsid`, `nohup` and `disown` deliberately escape the cleanup that
  closes your shells, so what they start is nobody's to stop.
- Never start a second Thetis. To see a UI change use the preview above; if you
  genuinely need an isolated instance, ask the user rather than launching one
  from a terminal.

## Scratch space

`workspace/` is shared by every conversation and is not in git — good for
handing a file to the user, bad for anything you rely on. For scratch that is
yours alone, use a path inside your own worktree.
