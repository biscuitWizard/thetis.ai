---
name = "The turn lifecycle"
brief = "What happens in one agent turn: rehydration from the event log, prompt assembly, streaming, tool dispatch, nudges, and resume after a restart."
when_to_use = "Use when you must understand or change how a turn runs: the order of the steps in handle-turn, how messages are rebuilt from the log, what stops a turn, how a nudge or a cancel arrives mid-turn, how usage and cost are counted, or why a turn continues after a restart. Not for compaction internals, which have their own child skill."
universal = false
tags = ["turn", "loop", "rehydrate", "event log", "nudge", "cancel", "streaming", "tool dispatch", "resume", "session"]
version = 1
---

# The turn lifecycle

All of this is in `agents/agent-core/src/lib.rs`. The struct is `Turn`. The
export is `handle-turn(session-id)`.

## The steps

1. **`Turn::new`** — Read the session record. The choices of the session win
   over the defaults of the grip: `model`, then `mode`. An empty value means
   "use the default".
2. **`rehydrate`** — Rebuild `self.messages` from the event log. See below.
3. **`retrieve_skills_once`** — If the session has no pinned skills, rank the
   corpus against the first user message and pin the result. Then rehydrate
   again, because the system prompt is now different.
4. **`maybe_compact`** — See the `compaction` child skill.
5. **The loop**, for each iteration:
   a. `stream_completion` — Send `model`, `messages` and `tools`. Read chunks
      until `Finished`. Each `Delta` chunk goes to the browser immediately with
      `emit_output`, and also into the reply text.
   b. Append an `AssistantMessage` event **before** you act on it. The log must
      be true even if a tool traps.
   c. `record_usage` — Add the prompt tokens, completion tokens and cost.
   d. If there are no tool calls, drain the inbox. `None` stops the turn.
      `Nudged` continues it. `Cancelled` stops it.
   e. If there are tool calls, dispatch each one, then drain the inbox for a
      cancel.
6. Return `TurnStats`: iterations, tokens, cost, the tools used, and
   `stopped_by`.

`stopped_by` is one of `stop`, `max-iterations`, `cancelled`, or `llm-error`.

Note: `max_iterations` in the `Turn` struct is set to `u32::MAX`, so the
iteration ceiling in the config is not applied by the current loop code. Verify
this in the source before you depend on either behaviour.

## How rehydration works

`rehydrate` walks all events of the session in order and maps each one to a
message.

| Event | Becomes |
|---|---|
| `UserMessage` | `user`, with attachments as `image_url` parts |
| `Nudge` | `user` |
| `AssistantMessage` | `assistant`, with `tool_calls` when present |
| `ToolResult` | `tool`, with `tool_call_id` |
| `SystemNote` | `user`, with a `[system note]` prefix |
| `ContextCompacted` | A summary note, in place of the spans that it covers |
| All other events | Nothing. They are bookkeeping. |

Two rules that look strange but are necessary:

- A `SystemNote` becomes a **user** message, not a system message. A note is
  appended wherever the log is at that moment. Anthropic refuses a `system`
  message that is in the middle of the array.
- The summary note of a compaction also takes the **user** role. User messages
  are never eligible for compaction, so a summary can never be summarized
  again.

`self.origins` holds the log sequence number of each message, in step with
`self.messages`. `push()` adds to both lists together so that they cannot get
out of step. Compaction needs this: it records spans against the log, not
against this rebuild of the log. If the two lists have different lengths,
compaction refuses to run.

`context_tokens` comes from the `prompt_tokens` of the **last**
`AssistantMessage` in the log. It is the count of the provider for everything
that it received: the system prompt, the tool schemas, and all history. This is
the trigger value for compaction. Do not replace it with an estimate.

## The system prompt

`system_prompt()` puts these parts together, in this order:

1. The base prompt, from the config key `system_prompt`.
2. The prompt of the active mode, if the mode has one, under a
   `# Mode: <label>` heading.
3. `# Always-available skills` — the brief of each universal skill.
4. `# Skills retrieved for this conversation` — the cards that retrieval added,
   with `when_to_use` and any children. Universal skills are filtered out of
   this part, because they are already above it.

No skill body is ever in the prompt. That is the function of `skill_fetch`.

Keep this function stable. A prompt that changes between turns destroys the
prompt cache.

## Nudges and cancels

`drain_inbox` reads `poll_inbox`. An `InboxItem` is one of:

- `Nudge(text)` — New text from the user while the turn runs. It is pushed as a
  `user` message with origin `0`, because the host already logged it and the
  copy in the turn has no sequence of its own.
- `Cancel` — Stop the turn. A cancel always wins over a nudge.
- `Control(cmd)` — Logged and ignored.

One session has one actor task, in `crates/thetis/src/session.rs`. Therefore a
message that arrives during a turn becomes a nudge in that turn. It does not
start a second turn that races the first.

## Resume after an interruption

A turn that stops early — from `restart_orchestrator`, a crash, or a trap — is
run again when Thetis comes back. This is possible because the agent is
stateless.

The host does two things first:

1. It answers each tool call that has no result with a failure. Most providers
   refuse a request that has a tool call with no matching result.
2. It appends a note that explains the interruption.

A turn that continues to fail stops being resumed after a small number of
attempts. The count is cleared when a turn reaches its end.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| The turn returns `llm-error` | The provider refused or the transport failed | Read the message. `describe_llm_error` names the class: auth, rate limit, transport, model, budget, or bad request. |
| The provider refuses the request | A `tool` message has no assistant turn before it, or a `system` message is in the middle | Look at the mapping table above. Do not put a `system` role in the middle. |
| The cost of each turn is high and there is no cache hit | The system prompt changed between turns | Compare the output of `system_prompt()`. Retrieval must be pinned, not run again. |
