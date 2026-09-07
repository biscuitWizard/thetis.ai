---
name = "Asking the user structured questions"
brief = "How the ask_user tool renders as a form in the web transcript and as components in Discord, and the invariants each surface must keep."
when_to_use = "Use when changing the ask_user tool, the transcript form it draws, or the Discord question flow — or when adding structured questions to a new chat surface. Also use when an ask_user form renders wrong, cannot be answered, can be answered twice, or when a Discord interaction gets no reply. Not for ordinary tool authoring, and not for the composer or normal message rendering."
universal = false
tags = ["ask_user", "questions", "choices", "multiple choice", "form", "discord components", "select menu", "modal", "transcript", "interaction"]
version = 2
---

# Asking the user structured questions

`ask_user` lets a turn pose several questions — multiple choice or open — and
stop. It exists on three surfaces at once, and the surfaces share no code, only
invariants. Break one and the failure is a user who cannot answer.

## The shape of the design

| Piece | Where | Job |
|---|---|---|
| The tool | `agents/agent-core/src/tools.rs` (`fn ask_user`) | Validate, return prose |
| The loop | `agents/agent-core/src/lib.rs` (`fn run`) | End the turn when the call succeeded |
| Web form | `gateways/gateway-web/src/ui/views/askuser.js` | Draw a card in the transcript |
| Discord | `crates/thetis/src/discord/ask.rs` + `mod.rs` | Post components, collect, resubmit |

**No WIT change is needed and none should be added.** `SessionEvent::ToolInvocation`
is already appended and broadcast, and `render.rs` already puts `name` on the
`tool-call` frame. That `name` is the whole hook: each surface special-cases
`name == "ask_user"` and builds the UI from `arguments_json`. A new wire frame
would buy nothing and would have to be versioned forever.

## The five invariants

These hold on every surface. Each one is a real failure that was designed out.

1. **The tool returns at once and the loop ends the turn.** `ask_user` validates,
   returns, and then `run()` in `agents/agent-core/src/lib.rs` breaks out of the
   loop with `stopped_by = "asked"`. The answer arrives as the user's *next*
   message, starting the next turn.

   The enforcement is the point. This used to rest only on the result text
   saying "End your turn now", and an instruction is not a guarantee — the model
   would carry on past its own questions and sometimes answer them itself. The
   loop now decides, so the pause holds whatever the model intended. Two details
   that matter if you touch it:

   - Only a **successful** call pauses. A malformed one was rejected and never
     shown, so pausing would hang the conversation on the model's own mistake.
     `dispatch` returns the outcome for exactly this.
   - The inbox is **not** drained on the way out. Text the user typed while the
     questions were being posed stays queued, so the session actor starts a
     follow-up turn for it; draining would swallow it.

   An earlier version of this file claimed a blocking tool "would deadlock".
   That is false, and worth knowing so nobody rejects the idea for the wrong
   reason: host imports are async and the session actor selects on `rx.recv()`
   concurrently with the turn future, so a parked guest still receives nudges,
   and the epoch timer cannot trap it because epoch interruption only fires
   while the guest executes wasm. Blocking is *possible*. It is not done because
   it would cost real safety: the inbox has no `Notify` to wake on, a restart
   mid-wait answers the unanswered call with a failure stub and kills the form,
   Discord's resubmit flow would need reworking, and a turn parked on a human
   holds a worker open indefinitely. Ending the turn keeps all of that free.
2. **Every choice question ends with a free-text option.** A list the model
   wrote is a guess about the answer space; this is how the user disagrees with
   it. Added by the *renderer*, never by the caller, so no surface can offer a
   question with no escape.
3. **Every question can be skipped**, and so can all of them at once.
4. **Answers go back as an ordinary user message.** Composed as numbered prose
   that restates each question, because the model sees only text and cannot see
   the form's structure. This is also what makes it replay-safe: the answers are
   in the event log as text, so a reloaded transcript needs no special state.
5. **`mutating: false`.** It changes nothing outside the conversation, and
   read-only modes — Discord's, notably — are exactly where asking matters most.
   This flag is why Discord may use it at all.

## Web surface

The form lives **inside the transcript**, not in the rail and not in a modal.
The questions are a message; they belong in the reading order of the messages
that motivated them. A rail tab would separate question from context, and a
modal would hide the conversation the user needs in order to answer.

Wiring, in `transcript.js`:

- `RENDERERS["tool-call"]` returns early when `ev.name === "ask_user"` and
  `askRow(ev)` succeeded; an unparseable call falls through to the ordinary tool
  row rather than vanishing.
- `"tool-result"` **skips** the matching result when the call rendered as a form,
  otherwise the card is followed by a redundant "presented; answers arrive
  next" row.
- `RENDERERS.user` calls `lockAsks()` first, so any open form is retired the
  moment a user message lands. That is what stops a replayed transcript from
  offering the same form twice.
- `onAnswer(text)` returns whether the socket took it. Returning `false` leaves
  the form **usable** — a dead socket must not swallow what someone typed.

Two DOM traps that cost time here:

- A `<textarea>`'s content is its child text, **not** a `value` prop. `el()`
  passes unknown props to `setAttribute`, where `value` is silently ignored, and
  a restored draft disappears.
- The DOM holds checked state independently of your model, so a Skip toggle must
  clear the inputs itself, not just the model object.

## Discord surface

`ask.rs` is pure formatting and state — no socket code — which is why it carries
~16 unit tests and needs no token to verify.

**One question per message.** Discord allows 5 action rows, a select menu eats a
whole row, and a text input is legal *only* inside a modal. So the flow is: post
question N with components, edit the same message as answers arrive, retire the
controls when done, then submit the composed answers as one user message.

Limits that shape the rendering (all enforced in `ask.rs`):

| Thing | Cap |
|---|---|
| Select menu options | 25 — hence `OPTIONS_MAX = 23` plus Other and Skip |
| `custom_id` | 100 chars |
| Button label | 80 |
| Option label / placeholder | 100 / 150 |
| Modal label | 45 |

≤2 options renders as buttons; more renders as a select menu. Free text opens a
modal (callback type 9), because there is nowhere else Discord will accept typed
input.

Interaction plumbing in `api.rs`:

- `parse_interaction` handles **only** type 2 (APPLICATION_COMMAND).
  `parse_component` handles type 3 (MESSAGE_COMPONENT) and type 5
  (MODAL_SUBMIT), producing `Event::Interacted(Component)`. The two are disjoint
  by type, so a payload can never match both.
- Interaction callbacks authenticate by the token **in the path** and must carry
  no Authorization header — add one and you get a 401 that reads exactly like a
  bad bot token. Ordinary channel endpoints do need `Bot` auth.
- A modal submission carries no message to update, hence the split path:
  `ack_interaction` (type 6) then `edit_with_components`, versus
  `update_interaction_message` (type 7) for a component click.

Rules `handle_component` in `mod.rs` must keep:

- **Re-authorize.** Component interactions never pass through `policy::decide`,
  so omitting this makes the form answerable by anyone who can see it. Check
  both `discord.authorized(...)` and that the clicker is `state.user_id`.
- **State in the KV store, not memory.** Workers and the orchestrator restart;
  a form outliving its state is a form that cannot be answered. The KV interface
  has no delete, so clearing writes `""`, which fails to deserialize and reads
  back as absent.
- **Save state before posting.** A message whose state was never stored is a
  dead form.
- **Answer stale clicks kindly.** `route.index != state.index` means an edited
  older message; reply ephemerally rather than recording the wrong answer. An
  expired or missing form retires its controls.
- **Never add a way to change the session mode.** The read-only guarantee is the
  mode; see `discord-connector`.

## Verifying

- Tool and Discord: `cargo test -p thetis --lib discord`. `ask.rs`'s tests cover
  custom_id round-trips, the option cap, control retirement, stale indices,
  char-boundary truncation and the free-text/skip guarantee on both renderings.
- The tool's own validation: `cargo test` in `agents/agent-core`, module
  `ask_user_tests`. These pin the wire name and check that every malformed shape
  returns `Err`, which is what stops a rejected call from pausing the turn.
  Do **not** call `available()` from a host test: building the tool list reads
  capability flags through host imports that do not exist outside wasm, and the
  process aborts with SIGABRT rather than failing a test.
- Web: the live port serves trunk's UI, so use `/preview/<full session id>/` —
  the **full** uuid, not the branch's short suffix, or you get "has no branch
  yet, so it has nothing to preview". Then drive headless Chrome over CDP and
  `await import('./views/askuser.js')` in the real page, so production CSS and
  `dom.js` are what you are testing. See `web-ui-design/verifying-on-a-branch`.
- Run one probe at a time. Three CDP probes sharing one browser produced a
  spurious horizontal-overflow failure at a single width.

## Failure branches

| Symptom | Cause |
|---|---|
| Form renders, then a redundant tool row follows | The `tool-result` arm is not skipping the answered call |
| The same form can be answered twice after reload | `RENDERERS.user` is not calling `lockAsks()` |
| A typed draft vanishes on re-render | `value` passed as a prop to `<textarea>` |
| Discord click does nothing, no error | `parse_component` not reached — check the INTERACTION_CREATE dispatch order |
| Discord replies 401 to a callback | An Authorization header on an interaction callback |
| Form unanswerable after a restart | State kept in memory instead of KV |
| Tool missing in a read-only mode | `mutating` set to true |
| The model asks, then keeps working or answers itself | The loop is not breaking on `asked` — check `run()` still matches `tools::ASK_USER` |
| A malformed call pauses the turn anyway | The `asked` flag is being set without checking `dispatch`'s return |
| A mid-turn message is silently lost after asking | Something drained the inbox on the `asked` path |
| Turn footer badges every ask as an anomaly | A surface is not treating `stopped_by == "asked"` like `"stop"` |
| `/preview/` 404s with "no branch yet" | Short session id instead of the full uuid |
