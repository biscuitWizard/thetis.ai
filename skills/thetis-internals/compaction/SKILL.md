---
name = "Context compaction"
brief = "How a long conversation is summarized down: the trigger, the head and tail that stay, rounds and spans, and why the originals are never deleted."
when_to_use = "Use when the context is near its limit, when a summary note appears in the history, or when you must change or debug compaction: the trigger threshold and target, keep_head and keep_tail, round and span selection, the summary model, or the ContextCompacted event. Not for the wider turn loop, which is in the turn-lifecycle child."
universal = false
tags = ["compaction", "context window", "summary", "token limit", "keep_head", "keep_tail", "rounds", "spans", "long conversation", "tool-group:selfmod"]
version = 1
---

# Context compaction

The code is `agents/agent-core/src/compaction.rs`. The agent calls it from
`maybe_compact` in `lib.rs`, before the first completion of the turn. The turn
that is about to run is therefore the turn that gets the benefit.

## The most important property

**Compaction never edits the log.** The log stays append-only. A compaction
appends one `ContextCompacted` event that records *which spans a summary stands
for*. Rehydration then projects the log through those records: it skips the
covered events and puts a note in their place.

The original events are still in the log. If a detail is missing from a summary,
read the log. This is why there is no separate offload store.

## The trigger

`Policy::load()` reads these values from the host with `config_get`. The host
key names are different from the config file names.

| Policy field | Host key | Config file key |
|---|---|---|
| `enabled` | `compact_enabled` | `context.enabled` |
| `window` | `context_window` | `context.window_tokens` |
| `threshold` | `compact_threshold` | `context.compact_threshold` |
| `target` | `compact_target` | `context.compact_target` |
| `summary_model` | `summary_model` | `context.summary_model` |
| `keep_head` | `keep_head` | `context.keep_head` |
| `keep_tail` | `keep_tail` | `context.keep_tail` |

Compaction starts when `context_tokens > window * threshold`. It tries to get to
`window * target`. An unknown context size, which is `0`, is not a reason to
compact. An empty `summary_model` means the host gives the main model.

Read the current values with `read_config`. Do not trust a number that is
written here.

## What is protected

The selection rules matter more than the summary prompt. These parts are never
summarized:

- **Index 0**, the system prompt. It is not part of the conversation.
- **The head**, the first `keep_head` messages after the system prompt. The head
  holds the framing that everything after it refers back to.
- **The tail**, the last `keep_tail` messages. The tail is your live working
  memory. If you collapse it, the next turn is incoherent.
- **Every user message, anywhere.** They are the steering of the human, they are
  short, and a paraphrase loses the instruction. They are also the natural break
  points between rounds.

`eligible_middle` also moves the edges. It moves the low edge forward past a
`tool` message whose assistant turn is in the protected head, because a summary
of that message alone would orphan it. It moves the high edge back so that the
tail does not begin on a bare `tool` message.

## Rounds, selection, spans

1. **`rounds`** cuts the eligible middle into the smallest self-contained units.
   A round is one assistant turn with tool calls plus the `tool` results that
   answer it, or one standalone assistant message. A round is never split.
2. **`select`** takes rounds oldest first. It stops at the first round boundary
   that meets `need`, where `need = context_tokens - target_tokens`. A long
   conversation therefore sheds what it must and keeps the rest.
3. **`spans`** joins the selected rounds into contiguous groups. A gap — a
   protected user message between two selected rounds — breaks the group. No
   summary ever crosses something the human said.

`est_tokens` is characters divided by four. It is used only to compare messages
with each other. The trigger uses the real count of the provider.

## The summary

Each span becomes one call to the summary model, with `SUMMARY_INSTRUCTIONS`.
The instructions ask for four sections: session intent, progress so far, key
facts learned, and open threads. They forbid invention and editorialising.

If a summary call fails, the span is left alone. An unsummarized span costs
context. A wrong summary costs correctness.

The note that replaces a span says how many messages it replaced and which event
sequence numbers they were, and it says that the originals are still in the log.

## Progress, and stopping

A compaction is several back-to-back summary calls before the turn's first
completion. It used to be tens of seconds of total silence — no tokens, no tool
rows, a stop button that did nothing — which is indistinguishable from a hang.

`plan` now only selects; `run` does the calls and reports. Both take
`session_id` and emit `compaction-progress` frames through
`session.emit-compaction-progress`. The frame carries `phase`
(`planning` | `summarizing` | `finished` | `failed` | `cancelled`), `span` /
`spans`, `messages`, `tokens-before` / `tokens-target`, `model` and a
human-readable `detail`.

**The frames are transient, like `stream-delta`.** They are never persisted:
the log already records the outcome as `ContextCompacted`, and a progress
stream in the log would be a hundred near-identical rows. The consequence is
that a page reloaded mid-compaction shows no card — correct, not a bug.

The web gateway renders them as kind `"compacting"`, and `transcript.js` keeps
a single `compactNode` card under the last message, updated in place. Anything
that ends the attempt clears it: `compacted`, `incident` and `turn-finished`.
That last one matters, because a stop landing *inside* a summary call traps the
guest via `budget.cancel()` and no `cancelled` frame is ever emitted.

Stopping works on two levels, and both are needed:

- `run` calls `stop_requested` before each span, so a stop during span one is
  not answered by four more requests. A nudge found there is carried back to
  the caller and pushed onto `pending` — consuming it here would lose it.
- The host's `llm::chat` is wrapped in `interruptible`, because it was a single
  await lasting tens of seconds with no stop checkpoint at all. `stream_next`
  had its own race; the non-streaming path had nothing.

`maybe_compact`'s caller also checks `stopped_before_starting()` before entering
the loop, so a turn stopped during compaction ends with `stopped_by =
"cancelled"` and zero iterations rather than proceeding to a completion.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Nothing is compacted although the context is large | The middle is not eligible, or every round is protected | Check `keep_head` and `keep_tail` against the conversation length. A short conversation has no eligible middle. |
| The log says "message and origin lists disagree" | A `push` did not add to both lists | Find the code path that added a message without a sequence. Compaction refuses rather than guess, because a panic would trap the turn. |
| A summary got summarized | A note did not take the `user` role | Notes must be `user` role. Check `compaction::note`. |
| The UI goes silent and stop does nothing for tens of seconds | A summary call is not interruptible | Check `llm::chat` is still wrapped in `interruptible` in `host_api.rs`. |
| The progress card spins forever | Nothing cleared `compactNode` | Every terminal path must clear it — `compacted`, `incident`, `turn-finished`. |
| No card appears at all after a reload | Progress frames are transient by design | Expected. The `compacted` event is the durable record. |
| The provider refuses the request after a compaction | A span split a tool round | Check `rounds` and `eligible_middle`. A round must stay whole. |
