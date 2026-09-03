---
name = "Sub-agents and delegation"
brief = "How an agent spawns sub-agents, waits on them, and how a child's work is isolated, routed, settled and shown in the UI."
when_to_use = "Use when working on sub-agent delegation in Thetis: the spawn_agent/wait/agent_status/agent_transcript/cancel_agent tools, the `delegation` WIT interface, the subagents registry, agent profiles in [subagents], child-session routing and settlement, or the nested sub-agent block in the transcript. Use it also to explain to a user why a child cannot delegate, why a wait always has a deadline, or where a child's cost is counted. Not for the ordinary single-agent turn loop, which is thetis-internals/turn-lifecycle, and not for the general procedure for editing yourself, which is careful-surgery."
tags = ["subagent", "subagents", "delegation", "spawn", "fan-out", "parallel", "concurrency", "wait", "child session", "orchestrator-worker"]
version = 1
---

# Sub-agents and delegation

A sub-agent is a **child session** that a parent agent briefs and starts. The
parent keeps working; the child's answer comes back through `wait`.

## The one idea

What is isolated is **context**, not the filesystem.

A child shares its parent's worker, branch and worktree. It gets a fresh
conversation with only the brief in it. That is the whole payoff: the parent
spends a few hundred tokens on a brief and a few hundred on an answer, instead
of the tens of thousands the work took.

The corollary is the rule that governs every design decision here: **the child
cannot see the parent's conversation.** A brief that says "look into that bug"
gives the child nothing.

## The invariants

Break one of these and the feature is unsafe, not merely wrong.

1. **One level of nesting.** A sub-agent cannot delegate. Enforced twice:
   `delegation::available()` returns false for a child so the tools are
   withheld, and `spawn` refuses if the caller is already in the registry.
2. **Fan-out is capped on *live* children**, not on children ever spawned. A
   parent that has finished ten children in sequence is not near the cap.
3. **A wait always has a deadline.** A wait that could run forever cannot be
   told apart from a hung turn. The host clamps to `max_wait_secs` regardless of
   what was asked.
4. **A child never asks a question.** Its brief says so. `ask_user` from a child
   would block on a form no one is watching; the turn ends `asked`, which
   settles as a failure.
5. **A child does not commit the worktree.** Several children ending at once
   would race on one tree. Only the parent's turn checkpoints.
6. **An empty answer is a failure.** A child that finished with nothing to say
   did not succeed, and the parent must be able to tell.
7. **Children are hidden from the session list.** They are not conversations;
   they appear inside their parent's transcript.

## The pieces

| Concern | Where |
|---|---|
| Registry: rows, states, depth guard, fan-out cap | `crates/thetis/src/subagents.rs` |
| Behaviour: spawn, wait predicates, cancel, settle | `crates/thetis/src/delegation.rs` |
| Host imports the agent calls | `crates/thetis/src/host_api.rs`, `impl delegation::Host` |
| Contract | `wit/thetis.wit`, `interface delegation`, imported by `world agent` |
| Routing a child to the parent's worker | `crates/thetis/src/workers.rs`, `routing_key` |
| Settling a child at the end of its turn | `crates/thetis/src/session.rs` |
| Tagging a child's live frames | `crates/thetis/src/roles/worker.rs`, `tag_frame` |
| Merging a child's log on replay | `gateways/gateway-web/src/handlers.rs`, `history` |
| Nested rendering | `gateways/gateway-web/src/ui/views/transcript.js` |
| Tools and profiles the model sees | `agents/agent-core/src/tools.rs`, `subagent_tools` |
| Which group each tool is in | `agents/agent-core/src/groups.rs` — `subagents`, but `wait` is in `core` |
| Configuration | `thetis.toml`, `[subagents]` and `[[subagents.profiles]]` |

## How one child's life runs

1. The parent calls `spawn_agent`. The host resolves profile, then mode and
   model by precedence: explicit argument, then profile, then the grip default.
2. A session is created, registered in the `subagents` table, and briefed. The
   brief is the profile prompt, plus the task, plus a section stating that the
   final message is all the parent will see.
3. `spawn` returns as soon as the turn has *started*. Blocking delegation is
   spawn followed by `wait` on that one child — there is no separate blocking
   call, and adding one would only duplicate this.
4. `routing_key` resolves the child to its root, so the child's turn runs in the
   parent's worker against the parent's worktree.
5. When the child's turn ends, `session.rs` takes the last non-empty assistant
   message as the answer, clamps it, writes cost and state to the registry,
   notes it in the parent's transcript, and rings the settle bell.
6. The parent's `wait` wakes on the bell — with a 2s poll as a backstop, because
   the bell is worker-local and advisory.

## Wait predicates

`wait` is a **core** tool, in the `core` group, not in `subagents`. That is
deliberate and load-bearing: a sub-agent is refused the delegation group
outright, and a sub-agent is exactly the session most likely to be handed a long
build to babysit. If `wait` travelled with the group it would be the one kind of
session unable to sleep, and it would poll instead — an iteration and a slice of
context each time round. `groups::tests::wait_is_core_and_not_a_sub_agent_tool`
holds this in place.

The predicates that name children simply have nothing to match for a session
with none; the host answers "no such sub-agents". Only `time` is universal, and
it is the one every session can reach.

`until` is one of:

| Value | Ends when |
|---|---|
| `time`, `duration` | the timeout elapses — a plain sleep |
| `all`, `all_children` | every named child has finished |
| `any`, `any_child` | one named child has finished |
| `first_failure` | a named child failed or was cancelled |

`children` names which to watch; empty means all of them. An empty selector with
no children returns immediately rather than hanging.

The defaults are chosen from the situation, which saves a refused call: with
children running, a bare `wait` means `all`; with none, the only sensible
reading is a sleep, and a sleep must carry `timeout_secs`.

`first_failure` is the one worth reaching for deliberately. It is how a parent
stops paying for nine children when the tenth has already invalidated the plan.

## Failure modes to design against

These come from the multi-agent literature (Cemri et al., *Why Do Multi-Agent
LLM Systems Fail?*, arXiv 2503.13657) and each has a countermeasure here:

| Failure | Countermeasure |
|---|---|
| Under-specified brief | `check_brief` refuses a brief under 40 characters, and says why |
| Information withheld in a handoff | The answer is rendered inline in `wait`, so the parent cannot miss it |
| Premature termination | An empty answer settles as `failed`, never `done` |
| No verification of a child's claim | `agent_transcript` exists so the parent can check the work behind an answer |
| Unbounded spawn | `max_children`, on live children |
| Cascading failure | Cancelling the parent's turn cancels its children |

## Adding a profile

A profile names a model and a mode and carries a prompt. In `thetis.toml`:

```toml
[[subagents.profiles]]
id = "scout"
label = "Scout"
description = "Reads and reports. Cannot write."
model = "anthropic/claude-sonnet-5"
mode = "plan"
prompt = "..."
```

Validated at startup: the `model` must be in `[[models]]` and the `mode` in
`[[modes]]`, so a typo fails the start rather than the spawn. A read-only
profile is just one whose mode is read-only — no separate mechanism.

## Verifying a change here

The registry and predicates have unit tests:

```
cargo test -p thetis --lib -- delegation:: subagents::
cd agents/agent-core && cargo test
```

Two traps:

- **`/preview/` does not work on a branch that changed the WIT contract.**
  `preview_component` keys the buildcache by `kernel_wit_fingerprint()` — the
  *running* kernel's compiled-in contract — so a branch with a different
  contract never gets a cache hit. Verify the transcript module under Node with
  a DOM stub instead; there is a working harness at
  `/opt/thetis/workspace/subagent-ui/`.
- **Do not run `cargo test -p thetis` with `THETIS_BIND` set** in the shell. It
  overrides the value a settings test writes and fails
  `a_change_that_would_not_load_is_refused`. Use `env -u THETIS_BIND`.

The nesting logic is worth testing against interleaved children specifically: a
child numbers its log from 1, so two children produce colliding `seq` values and
colliding tool-call ids, and flat-keyed state will cross their streams.
