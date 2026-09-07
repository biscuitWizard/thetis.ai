---
name = "Thetis internals"
brief = "How you work inside: host and guest components, the turn loop, context compaction, skills, config, and where each source file is."
when_to_use = "Use when the task needs knowledge of your own machinery: your architecture, the agent loop, message rehydration, context compaction, the skill system, the WIT contract, your sandbox branch and its history, config keys, or the location of a source file. Use it also to explain yourself to a user. Do not use it for a user's own project code, and do not use it as the procedure for a self-edit; that procedure is in careful-surgery."
universal = false
tags = ["thetis", "architecture", "self-knowledge", "agent loop", "compaction", "skills", "wit", "orchestrator", "internals", "how do you work", "tool-group:selfmod"]
children = "auto"
related = ["careful-surgery", "skill-creator"]
version = 2
---

# Thetis internals

This skill is written in ASD-STE100 Simplified Technical English.

## The two parts of the system

One native process, the **orchestrator**, is the trusted kernel. It holds all
authority: the network, the disk, the database, and the build toolchain. It
gives each guest a narrow slice of that authority. The slices are the imports in
`wit/thetis.wit`. A guest has no other power.

A guest is a WebAssembly component in a **aspect**. An aspect is one hot-swappable
position in the running system.

| Aspect | Source directory | Function |
|---|---|---|
| `agent` | `agents/agent-core` | Runs the turn loop. This is you. |
| `gateway:<name>` | `gateways/gateway-<name>` | Serves the chat UI and the wire protocol. |
| `tool:<name>` | `tools/<name>` | One tool each. You scaffold and edit these. |

The host makes a new instance of a guest for each call, in a new store. No guest
state stays alive between calls. Therefore a hot swap is always safe: a call
that is already in flight keeps the old component and completes on it. The next
call gets the new component.

A change to a tool is callable on the next loop iteration. A change to
**yourself** takes effect when the current turn ends. A change to the kernel or
to the contract needs a restart.

## Design intents

These are the invariants of the system. Keep them true. If a change breaks one,
the change is wrong.

1. **The agent holds no state between turns.** Each turn rebuilds the
   conversation from the event log. A crash, a hot swap, or a restart then costs
   nothing.
2. **The event log is append-only.** Nothing edits or deletes an event.
   Compaction changes only the projection of the log into messages.
3. **A guest holds no ambient authority.** If a capability is not an import in
   `wit/thetis.wit`, the guest cannot do it.
4. **No self-modification can make the system unreachable.** A candidate must
   compile, load, and answer a smoke test before it goes live. `/admin` has no
   WebAssembly in its path.
5. **The system prompt must be byte-identical between the turns of one
   conversation.** If it changes, the prompt cache of the provider misses and
   the cost increases about ten times. This is why the host pins the retrieved
   skills once for each session.
6. **A rebuilt message list must be a legal request.** Each `tool` message must
   come after the assistant turn that called it.
7. **Tell the user what a tool changes.** A read-only mode withholds every
   mutating tool, and also refuses it at dispatch.
8. **The compiler verdict comes back in the same tool result.** Correct your own
   build errors in the same turn, and never report a change you did not see
   build.
9. **A tool is offered only when the capability behind it exists.** You are
   never given a tool that must fail.
10. **A large corpus of knowledge must cost a constant amount of context.** Only
    briefs are always present. A body is pulled when it is wanted.

## Where to look first

| Question | Read this |
|---|---|
| How does one turn run? | `thetis-internals/turn-lifecycle` |
| Why did my context get smaller? | `thetis-internals/compaction` |
| How does skill retrieval work? | `thetis-internals/skill-system` |
| Which file holds this behaviour? | `thetis-internals/code-map` |
| How do I write or fix a tool? | `thetis-internals/tool-authorship` |
| How do I change a setting or restart? | `thetis-internals/config-and-recovery` |
| What may a guest do at all? | `references/wit-contract.md` |
| What are the current settings and tools? | `references/snapshot.md` |
| Can I trust a number written here? | `references/volatility.md` |
| How do I edit myself safely? | `careful-surgery`, a separate skill |

## Volatile data

Some facts change more quickly than this document. Do not trust a written list
of them. Get the current list from the tool that owns it.

| Volatile fact | Ask this tool, not this skill |
|---|---|
| Config keys and their values | `list_config`, `read_config` |
| Your available tools | The tool list in your own prompt |
| Model ids | `list_config` with prefix `models` |
| Mode ids | `list_config` with prefix `modes` |
| Revision numbers | `history` |
| Files in a component | `list_code`, `list_path` |
| Dependencies of an aspect | `list_dependencies` |
| Skills in the corpus | `skill_search`, `skill_lint` |

`references/snapshot.md` holds a dated copy of these lists for orientation only,
and each table in it gives the command that makes it again. If a snapshot and a
tool disagree, the tool is correct. `references/volatility.md` gives the rule for
which tier a fact is in, and therefore where it may be written.

`README.md` in the project root is a good introduction but it is not authority.
It already has one claim that the code no longer supports; `code-map` names it.
