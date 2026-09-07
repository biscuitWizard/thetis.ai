/* The message transcript.
 *
 * Every event kind gets an entry in RENDERERS, so supporting a new one is a
 * single function rather than a new branch in a growing switch.
 *
 * Assistant text renders as markdown once final; streaming stays plain text
 * because re-parsing on every token buys nothing. The transcript also keeps
 * the session's usage ledger (spend, tokens per turn) in the store, since the
 * events flowing through here are the only place those numbers appear.
 */

import { $, AGENT_NAME, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";
import { renderMarkdown } from "../lib/markdown.js";
import { askCard, parseAsk } from "./askuser.js";
import { turnAvatar as avatarFor } from "./avatars.js";

/** The tool whose call renders as a form instead of a tool row. */
const ASK_TOOL = "ask_user";

let root = null; // the scroll container
// Where rows are appended. Normally `root`, but a sub-agent's frames go into
// that sub-agent's own block instead. See `inAgent`.
let sink = null;
let live = null; // element collecting streamed tokens
let thinking = null; // element collecting streamed reasoning, if any

// Collapses the open reasoning block and stops collecting into it. Called when
// the answer starts or the turn ends, so thinking never absorbs later output.
function settleReasoning() {
  if (!thinking) return;
  const box = thinking.closest("details.reasoning");
  if (box) {
    box.removeAttribute("open");
    const label = box.querySelector("summary");
    if (label) label.textContent = "Thought for a moment";
  }
  thinking = null;
}
let onInspect = null; // opens the Context tab at the latest request
let onAnswer = null; // sends an ask_user form's answers as a user message
let pendingNode = null; // the optimistic row for a message not yet acknowledged
// The live compaction card, while one is running. Transient, like `live`: the
// progress frames are never persisted, so nothing replays it.
let compactNode = null;
// Ask forms still open. A user message means they were answered — during a
// replay as much as live — so they lock rather than invite a second answer.
let openAsks = [];
// Tool calls awaiting their result, by call id. A call and its result are one
// row, so the result has to find the row the call opened.
let open = new Map();

/** Tool output longer than this starts folded behind "show all". */
const RESULT_PREVIEW_CHARS = 4000;

/* --- sub-agents -------------------------------------------------------------
 *
 * A sub-agent is a separate conversation whose frames are re-addressed to the
 * conversation the user is watching, tagged with `agent`, `agent_label` and
 * `agent_parent`. Several can run at once, so their frames interleave: two
 * children streaming at the same time would otherwise splice their sentences
 * together in one column.
 *
 * So each child owns a *block*, and every renderer's append target is `sink`
 * rather than `root`. A tagged frame swaps in that child's block along with its
 * own streaming state — a child's half-written message must not be completed by
 * another child's `assistant` frame, and its tool rows must not be matched
 * against the parent's call ids. `inAgent` does that swap and always restores,
 * which is what keeps the parent's own rendering exactly as it was.
 *
 * The block is a `<details>`, open while the child works and folded when it
 * ends: a finished sub-agent's forty tool calls are the thing the parent
 * delegated in order *not* to read, and its answer has already been quoted back
 * into the parent's transcript. */
const agents = new Map();

/** A block per sub-agent, minted on its first frame and reused after. */
function agentBlock(id, label) {
  const found = agents.get(id);
  if (found) return found;

  const body = el("div", { class: "agent-body" });
  const block = el(
    "details",
    { class: "agent is-running", open: "", dataset: { agent: id } },
    el(
      "summary",
      {},
      el("span", { class: "agent-dot" }),
      el("span", { class: "agent-label" }, label || "sub-agent"),
      el("span", { class: "agent-state" }, "working"),
      el("span", { class: "agent-meta" }, "")
    ),
    body
  );
  root.append(block);

  // Each child keeps its own streaming cursors and its own open-call map, so
  // concurrent children cannot corrupt each other's rows.
  const entry = { block, body, live: null, thinking: null, open: new Map() };
  agents.set(id, entry);
  return entry;
}

/** Runs a renderer with a sub-agent's block and state swapped in. */
function inAgent(ev, render) {
  const entry = agentBlock(ev.agent, ev.agent_label);

  const outer = { sink, live, thinking, open };
  sink = entry.body;
  live = entry.live;
  thinking = entry.thinking;
  open = entry.open;

  try {
    render(ev);
  } finally {
    // Whatever the renderer left mid-stream belongs to this child.
    entry.live = live;
    entry.thinking = thinking;
    entry.open = open;
    sink = outer.sink;
    live = outer.live;
    thinking = outer.thinking;
    open = outer.open;
  }
  return entry;
}

/* Marks a child finished and folds it away.
 *
 * Driven from the child's own `turn-finished` frame rather than from anything
 * the parent reports, because that frame arrives whatever happens — a clean
 * stop, an error, a cancellation — and is the only signal that is never
 * missing. */
function settleAgent(entry, ev) {
  const failed = ev.stopped_by && ev.stopped_by !== "stop" && ev.stopped_by !== "asked";
  entry.block.classList.remove("is-running");
  entry.block.classList.toggle("is-bad", !!failed);
  entry.block.removeAttribute("open");

  const state = entry.block.querySelector(".agent-state");
  if (state) state.textContent = failed ? ev.stopped_by : "done";

  const bits = [];
  if (ev.iterations > 1) bits.push(`${ev.iterations} steps`);
  if (ev.cost > 0) bits.push(`$${ev.cost.toFixed(4)}`);
  if (ev.prompt_tokens > 0) bits.push(`${fmtK(ev.prompt_tokens)} tok`);
  const info = entry.block.querySelector(".agent-meta");
  if (info) info.textContent = bits.join(" · ");
}

export function mountTranscript(hooks = {}) {
  root = $("transcript");
  sink = root;
  onInspect = hooks.onInspect || null;
  onAnswer = hooks.onAnswer || null;
  showEmpty();
}

export function reset() {
  sink = root;
  live = null;
  thinking = null;
  pendingNode = null;
  compactNode = null;
  openAsks = [];
  open = new Map();
  agents.clear();
  clear(root);
  store.set({
    turnStats: [],
    liveTurn: null,
    spendSession: 0,
    lastUsage: null,
    lastModel: "",
  });
}

export function showEmpty(message = "Nothing here yet.", hint = "Send a message to begin.") {
  clear(root).append(
    el(
      "div",
      { class: "empty" },
      el("div", {
        class: "empty-mark",
        html: `<svg viewBox="0 0 32 32" width="34" height="34" aria-hidden="true">
                 <circle cx="16" cy="16" r="9" fill="none" stroke="currentColor" stroke-width="2"/>
                 <circle cx="16" cy="16" r="3" fill="currentColor"/></svg>`,
      }),
      el("div", { class: "empty-title" }, message),
      el("div", { class: "empty-hint" }, hint)
    )
  );
}

/** An interstitial for work that has to finish before there is a transcript. */
export function showWorking(message, hint) {
  clear(root).append(
    el(
      "div",
      { class: "empty" },
      el("div", { class: "empty-mark" }, el("span", { class: "spinner lg" })),
      el("div", { class: "empty-title" }, message),
      el("div", { class: "empty-hint" }, hint || "")
    )
  );
}

/* The optimistic row for a message the host has not echoed back yet.
 *
 * Everything else here is derived from the event log, and deliberately so — but
 * the log is silent between `submit` and the `user` event, which on a
 * conversation's first message is however long it takes to create a branch, a
 * worktree and a worker. Showing the text straight away, greyed and marked
 * "sending", is the difference between a UI that is working and one that looks
 * broken. It is replaced the moment the real event lands.
 */
export function showPending({ text, attachments, note }) {
  root.querySelector(".empty")?.remove();
  clearPending();
  pendingNode = el(
    "div",
    { class: "row user is-pending" },
    avatarFor("user"),
    el(
      "div",
      { class: "row-head" },
      "you",
      el("span", { class: "row-flag" }, "sending")
    ),
    text ? el("div", { class: "bubble-text" }, text) : null,
    pendingThumbs(attachments),
    el("div", { class: "pending-note" }, el("span", { class: "spinner" }), note || "")
  );
  // `root`, not `sink`: this is the user typing into *this* conversation, so it
  // is the one append in this module that must never be redirected.
  // belongs in the parent's column even if a sub-agent block happens to be the
  // current append target. A user never types into a sub-agent.
  root.append(pendingNode);
  toBottom(false);
}

/** Updates the pending row's explanation without rebuilding it. */
export function setPendingNote(note) {
  const line = pendingNode?.querySelector(".pending-note");
  if (!line) return;
  clear(line).append(el("span", { class: "spinner" }), note);
}

export function clearPending() {
  pendingNode?.remove();
  pendingNode = null;
}

/** Local attachments carry raw base64, not the data URL the wire form has. */
function pendingThumbs(attachments) {
  if (!attachments?.length) return null;
  const images = attachments.filter((a) => a.mime?.startsWith("image/"));
  const files = attachments.filter((a) => !a.mime?.startsWith("image/"));
  return el(
    "div",
    { class: "thumbs" },
    images.map((a) =>
      el("img", { class: "thumb", src: `data:${a.mime};base64,${a.data}`, alt: a.name, title: a.name })
    ),
    files.length
      ? el(
          "div",
          { class: "attached-files" },
          el("span", { class: "attached-label" }, `files attached (${files.length}):`),
          files.map((a) => el("span", { class: "thumb-file mono", title: a.name }, workspaceRel(a.name)))
        )
      : null
  );
}

/** True when the reader is following along at the bottom. */
function atBottom() {
  return root.scrollHeight - root.scrollTop - root.clientHeight < 140;
}

function toBottom(instant) {
  if (instant) {
    const previous = root.style.scrollBehavior;
    root.style.scrollBehavior = "auto";
    root.scrollTop = root.scrollHeight;
    root.style.scrollBehavior = previous;
  } else {
    root.scrollTop = root.scrollHeight;
  }
}

// --- pieces -----------------------------------------------------------------

/* One turn: an avatar in the gutter, a byline, then whatever the turn said.
 *
 * The avatar is a child of the row rather than of the byline, and CSS lifts it
 * out of flow into one of the transcript's two gutters — the agent's to the
 * left, yours to the right, which is also the side your byline aligns to. That
 * is what puts it outside the conversation instead of inline with the name, and
 * it means the byline's own layout is unaffected by whether a face is present.
 *
 * `role` picks the avatar as well as the colour — "user" and "assistant" are
 * the two that have a face, and any other role (a system note, a tool) gets no
 * avatar rather than a blank tile. */
function row(role, who, ...content) {
  const node = el(
    "div",
    { class: `row ${role}` },
    avatarFor(role),
    el("div", { class: "row-head" }, who),
    ...content
  );
  sink.append(node);
  return node;
}

function meta(content, tone = "") {
  sink.append(el("div", { class: `meta ${tone}`.trim() }, content));
}

function cut(text, max) {
  const flat = String(text ?? "").replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

/** Pretty-print JSON when it is JSON, and leave anything else alone. */
function pretty(raw) {
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

/** The gist of a call's arguments, short enough to sit on the summary line. */
function gist(raw) {
  if (!raw) return "";
  let value;
  try {
    value = JSON.parse(raw);
  } catch {
    return cut(raw, 80);
  }
  if (value === null || typeof value !== "object") return cut(value, 80);

  return cut(
    Object.entries(value)
      .map(([k, v]) => `${k}: ${cut(typeof v === "string" ? v : JSON.stringify(v), 40)}`)
      .join("  ·  "),
    90
  );
}

function section(label, body) {
  return [el("div", { class: "tool-label" }, label), el("pre", {}, body || "")];
}

/** A result section that folds anything enormous behind one click. */
function resultSection(content) {
  const text = String(content ?? "");
  if (text.length <= RESULT_PREVIEW_CHARS) return section("result", text);

  const pre = el("pre", {}, `${text.slice(0, RESULT_PREVIEW_CHARS)}\n…`);
  const more = el(
    "button",
    {
      type: "button",
      class: "tool-more",
      onClick: () => {
        pre.textContent = text;
        more.remove();
      },
    },
    `show all ${Math.round(text.length / 1024)} KB`
  );
  return [el("div", { class: "tool-label" }, "result"), pre, more];
}

/* One row per tool call.
 *
 * A call and the result it returns are one thing to a reader - what ran, and
 * what came back - so they share a row instead of stacking two cards. The row
 * opens on the call and is completed by the result, matched on the call id the
 * host puts on both.
 */
function toolRow(ev) {
  const node = el(
    "details",
    { class: "tool is-running" },
    el(
      "summary",
      {},
      el("span", { class: "tool-name" }, ev.name),
      el("span", { class: "tool-args" }, gist(ev.arguments)),
      el("span", { class: "tool-status" }, "running")
    ),
    ...section("arguments", pretty(ev.arguments))
  );
  sink.append(node);
  return node;
}

/** Fills in the row a call opened, or makes a standalone one if it is missing. */
function completeToolRow(ev) {
  let node = open.get(ev.id);
  open.delete(ev.id);

  if (!node) {
    // A result with no call in view: replaying a truncated log, or a call that
    // predates this connection. Better a row on its own than a dropped result.
    node = el("details", { class: "tool" }, el("summary", {},
      el("span", { class: "tool-name" }, ev.name),
      el("span", { class: "tool-args" }, ""),
      el("span", { class: "tool-status" }, "")));
    sink.append(node);
  }

  node.classList.remove("is-running");
  node.classList.toggle("is-bad", !ev.ok);
  const status = node.querySelector(".tool-status");
  if (status) status.textContent = ev.ok ? "ok" : "failed";
  node.append(...resultSection(ev.content));
}

function thumbs(attachments) {
  if (!attachments?.length) return null;
  const images = attachments.filter((a) => a.data);
  const files = attachments.filter((a) => !a.data);

  return el(
    "div",
    { class: "thumbs" },
    images.map((a) => el("img", { class: "thumb", src: a.data, alt: a.name, title: a.name })),
    // Named rather than merely listed: a file attachment is invisible in the
    // message text unless it was mentioned, and even then the reader should be
    // able to see at a glance that its *contents* went along.
    files.length
      ? el(
          "div",
          { class: "attached-files" },
          el("span", { class: "attached-label" }, `files attached (${files.length}):`),
          files.map((a) => el("span", { class: "thumb-file mono", title: a.name }, workspaceRel(a.name)))
        )
      : null
  );
}

/** Attachment names carry the `workspace/` prefix so the agent's file tools can
 *  use them verbatim. The reader does not need it repeated on every chip.
 *  (Verified after the trunk merge that restyled the transcript.) */
function workspaceRel(name) {
  return String(name ?? "").replace(/^workspace\//, "");
}

/* The user's own text with its @-mentions marked, so a sent message reads the
 * same way it did while being typed.
 *
 * Only tokens that match something actually attached are highlighted. A token
 * that never resolved is left plain, which is the honest rendering: it was
 * ordinary text as far as the message was concerned. */
function userText(text, attachments) {
  const attached = new Set((attachments || []).map((a) => workspaceRel(a.name).replace(/\/ \(listing\)$/, "/")));
  if (!attached.size) return el("div", { class: "bubble-text" }, text);

  const nodes = [];
  const token = /(^|[\s(\[])@([^\s@]+)/g;
  let at = 0;
  let match;
  while ((match = token.exec(text))) {
    const raw = match[2].replace(/[.,;:!?)\]}'"]+$/, "");
    if (!attached.has(raw) && !attached.has(`${raw}/`)) continue;
    const start = match.index + match[1].length;
    const end = start + 1 + raw.length;
    if (start > at) nodes.push(text.slice(at, start));
    nodes.push(el("span", { class: "mention-ref mono", title: `workspace/${raw}` }, text.slice(start, end)));
    at = end;
  }
  if (!nodes.length) return el("div", { class: "bubble-text" }, text);
  nodes.push(text.slice(at));
  return el("div", { class: "bubble-text" }, ...nodes);
}

function fmtK(n) {
  return n >= 10000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

// --- event renderers --------------------------------------------------------

/* Draws an ask_user call as a form.
 *
 * Returns false when the arguments cannot be read, so the caller falls back to
 * the ordinary tool row: a malformed call is still something the reader should
 * be able to see, and a silently missing row would look like a lost turn.
 */
function askRow(ev) {
  const ask = parseAsk(ev.arguments);
  if (!ask?.questions.length) return false;

  const card = askCard(ask, {
    onAnswer: (text) => {
      const sent = onAnswer?.(text);
      // No handler wired is a programming error, not a dead socket; treat a
      // missing hook as a failure so the form does not claim it sent.
      return sent === undefined ? false : sent;
    },
  });
  sink.append(card);
  openAsks.push(card);
  return true;
}

/** Locks every open form: the user has answered, so the controls are spent. */
function lockAsks() {
  openAsks.forEach((card) => {
    if (card.classList.contains("is-answered")) return;
    card.classList.add("is-answered");
    card.querySelectorAll("input, textarea, button").forEach((node) => {
      node.disabled = true;
    });
    const foot = card.querySelector(".ask-foot");
    if (foot) clear(foot).append(el("div", { class: "ask-note" }, "Answered."));
  });
  openAsks = [];
}

const RENDERERS = {
  user(ev) {
    live = null;
    // Whatever was asked has now been replied to, one way or another.
    lockAsks();
    row("user", "you",
      ev.text ? userText(ev.text, ev.attachments) : null,
      thumbs(ev.attachments));
  },

  delta(ev) {
    // The answer has begun, so the thinking is done: fold it away rather than
    // leaving a wall of reasoning above every reply.
    settleReasoning();
    if (!live) {
      const node = row("assistant", AGENT_NAME, el("div", { class: "bubble-text is-streaming" }));
      live = node.querySelector(".bubble-text");
    }
    live.textContent += ev.text;
  },

  // A reasoning model can spend most of its output thinking. Shown in its own
  // collapsible block so the wait is visible, but never mixed into the answer.
  reasoning(ev) {
    if (!thinking) {
      const box = el("details", { class: "reasoning", open: "" },
        el("summary", {}, "Thinking\u2026"),
        el("div", { class: "reasoning-text" }));
      row("assistant", AGENT_NAME, box);
      thinking = box.querySelector(".reasoning-text");
    }
    thinking.textContent += ev.text;
  },

  assistant(ev) {
    // The answer is here, so whatever thinking preceded it is finished.
    settleReasoning();
    // The final message is authoritative: it replaces whatever streamed (as
    // rendered markdown), so a reconnect part-way through still lands right.
    if (live) {
      live.classList.remove("is-streaming");
      live.classList.add("md");
      live.replaceChildren(...renderMarkdown(ev.text));
      live = null;
    } else if (ev.text?.trim()) {
      row("assistant", AGENT_NAME, el("div", { class: "bubble-text md" }, renderMarkdown(ev.text)));
    }

    // Each step reports its own usage, so the turn's cost can be watched as it
    // is spent instead of landing all at once when the turn ends.
    //
    // A sub-agent's steps are accounted by `dispatch` instead: its spend is
    // real and belongs to this conversation, but it retires on the *child's*
    // turn-finished rather than the parent's, and `lastModel` must keep naming
    // the model this conversation is using rather than whichever child last
    // spoke.
    if (ev.usage && !ev.agent) {
      store.set({ lastUsage: ev.usage, lastModel: ev.model || "" });
      const live = store.liveTurn || { cost: 0, prompt: 0, completion: 0, steps: 0 };
      store.set({
        liveTurn: {
          cost: live.cost + (ev.usage.cost || 0),
          prompt: live.prompt + (ev.usage.prompt || 0),
          completion: live.completion + (ev.usage.completion || 0),
          steps: live.steps + 1,
        },
      });
    }

    // Cache hits are invisible otherwise, and a saving you cannot see is one
    // you cannot trust.
    const cached = ev.usage?.cached ?? 0;
    if (cached > 0) {
      const total = ev.usage?.prompt ?? 0;
      const share = total > 0 ? Math.round((cached / total) * 100) : 0;
      meta(`${cached.toLocaleString()} of ${total.toLocaleString()} prompt tokens cached (${share}%)`, "is-good");
    }
  },

  "tool-call"(ev) {
    live = null;
    // The form stands in for the tool row entirely. Showing both would put the
    // same questions on screen twice, once as JSON.
    if (ev.name === ASK_TOOL && askRow(ev)) return;
    open.set(ev.id, toolRow(ev));
  },

  "tool-result"(ev) {
    // The result of an ask is only the note telling the model to stop and wait.
    // The form above says all of that to the reader already.
    if (ev.name === ASK_TOOL && !open.has(ev.id)) return;
    completeToolRow(ev);
  },

  /* Compaction, while it is happening.
   *
   * The card is transient and updates in place: the frames are a progress
   * stream, not a log, so appending a row per frame would bury the conversation
   * under a hundred near-identical lines. It sits under the last message, which
   * is where the reader is already looking, and it is the answer to a turn that
   * used to go completely silent — no tokens, no tool rows, a dead stop button —
   * for the tens of seconds several summary calls take. */
  compacting(ev) {
    live = null;
    root.querySelector(".empty")?.remove();

    if (!compactNode) {
      compactNode = el(
        "div",
        { class: "compacting" },
        el(
          "div",
          { class: "compacting-head" },
          el("span", { class: "spinner" }),
          el("span", { class: "compacting-title" }, "Compacting context"),
          el("span", { class: "compacting-count" }, "")
        ),
        el("div", { class: "compacting-track" }, el("span", { class: "compacting-fill" })),
        el("div", { class: "compacting-detail" }, ""),
        el("div", { class: "compacting-foot" }, "")
      );
      sink.append(compactNode);
    }

    const done = ev.phase === "finished" || ev.phase === "failed" || ev.phase === "cancelled";
    const spans = ev.spans || 0;
    // Planning has no span count yet, so the bar shows a sliver rather than
    // nothing: a 0% bar next to a spinner reads as stuck.
    const share = done ? 1 : spans > 0 ? Math.min(1, (ev.span || 0) / spans) : 0.06;

    compactNode.classList.toggle("is-done", done);
    compactNode.classList.toggle("is-bad", ev.phase === "failed" || ev.phase === "cancelled");
    compactNode.querySelector(".compacting-fill").style.width = `${Math.round(share * 100)}%`;
    compactNode.querySelector(".compacting-count").textContent =
      spans > 0 && !done ? `span ${ev.span || 0} of ${spans}` : ev.phase;
    compactNode.querySelector(".compacting-detail").textContent = ev.detail || "";

    const bits = [];
    if (ev.tokens_before > 0) {
      bits.push(`${fmtK(ev.tokens_before)} tokens → target ${fmtK(ev.tokens_target || 0)}`);
    }
    if (ev.messages > 0) bits.push(`${ev.messages} messages summarized`);
    if (ev.model) bits.push(ev.model);
    compactNode.querySelector(".compacting-foot").textContent = bits.join(" · ");

    // The spinner stops meaning anything once the run is over, and the
    // `compacted` event that follows carries the summary worth keeping.
    if (done) {
      compactNode.querySelector(".spinner")?.remove();
      const finished = compactNode;
      compactNode = null;
      // A run that produced nothing has no `compacted` event to replace this,
      // so it stays as the record of the attempt. One that succeeded is about to
      // be superseded, so it goes.
      if (ev.phase === "finished") setTimeout(() => finished.remove(), 400);
    }
  },

  compacted(ev) {
    live = null;
    // The progress card has said its piece; this row replaces it.
    compactNode?.remove();
    compactNode = null;
    // Foldable rather than a bare line: the summary is what the model now sees
    // in place of those messages, so it should be readable on demand.
    const node = el(
      "details",
      { class: "tool" },
      el(
        "summary",
        {},
        el("span", { class: "tool-name" }, "context compacted"),
        el(
          "span",
          { class: "tool-args" },
          `${ev.replaced} earlier messages summarized · was ~${(ev.tokens_before ?? 0).toLocaleString()} tokens`
        ),
        el("span", { class: "tool-status" }, "summary")
      ),
      ...section("summary", ev.summary)
    );
    sink.append(node);
  },

  nudge: (ev) => meta(`you interrupted: ${ev.text}`, "is-nudge"),
  note: (ev) => meta(ev.text),

  incident(ev) {
    live = null;
    // A turn that died mid-compaction leaves a card with a spinner on it and
    // nothing coming to finish it, which is the same lie this whole change is
    // about. Both endings below clear it for that reason.
    compactNode?.remove();
    compactNode = null;
    // An incident ends a turn. Without this, replaying a log that ends in one
    // would leave the composer showing "working…" with nothing running.
    store.set({ busy: false });
    meta(ev.text, "is-incident");
  },

  modification(ev) {
    const revision = ev.revision != null ? ` → r${String(ev.revision).padStart(4, "0")}` : "";
    const detail = ev.detail ? ` — ${ev.detail}` : "";
    meta(
      `${ev.ok ? "updated" : "could not update"} ${ev.aspect}${revision}${detail}`,
      ev.ok ? "is-good" : "is-incident"
    );
  },

  "branch-op"(ev) {
    live = null;
    const verbs = {
      update: "pulled trunk into this branch",
      merge: "merged to trunk",
      reset: "reset the branch",
      "resolve-handoff": "conflict resolution handed to this conversation",
      "merge-completed": "completed the merge",
      abort: "aborted the merge",
    };
    const short = (rev) => (rev ? rev.slice(0, 12) : "");
    const span =
      ev.from_rev && ev.to_rev && ev.from_rev !== ev.to_rev
        ? ` (${short(ev.from_rev)} → ${short(ev.to_rev)})`
        : "";
    const conflicts = ev.conflicts?.length
      ? ` — conflicts: ${ev.conflicts.join(", ")}`
      : "";
    const detail = ev.detail ? ` — ${ev.detail}` : "";
    meta(
      `${verbs[ev.op] || ev.op}${span}${detail}${conflicts}`,
      ev.ok ? "is-good" : "is-incident"
    );
  },

  // Deliberately does *not* clear `liveTurn`. A turn cut short by a restart
  // leaves spend that no ledger row covers yet — the host sweeps it into the
  // next row to be written — so clearing here made the header drop by whatever
  // the killed turn had cost, which is the "reset" this whole fix is about.
  // `liveTurn` therefore means "spend since the last ledger row", matching the
  // host's anchor exactly, and only a finished turn retires it.
  "turn-started": () => store.set({ busy: true }),

  "turn-finished"(ev) {
    store.set({ busy: false });
    settleReasoning();
    live = null;
    compactNode?.remove();
    compactNode = null;

    // The ledger the Usage view and the header's spend chip read. These totals
    // are authoritative for the turn, so the running tally it was accumulating
    // is dropped rather than added to them.
    // Keyed by sequence so a row cannot land twice. A frame can arrive both
    // live and again in a replay — a reconnect mid-turn does exactly that — and
    // an accumulating total counted the same turn twice when it did.
    const seq = ev.seq ?? `t${store.turnStats.length}`;
    if (!store.turnStats.some((t) => t.seq === seq)) {
      store.turnStats.push({
        seq,
        cost: ev.cost || 0,
        prompt_tokens: ev.prompt_tokens || 0,
        completion_tokens: ev.completion_tokens || 0,
        iterations: ev.iterations || 1,
        stopped_by: ev.stopped_by || "stop",
        ts: ev.ts,
      });
    }
    store.set({ liveTurn: null });
    recomputeTotals();

    const bits = [];
    if (ev.iterations > 1) bits.push(`${ev.iterations} steps`);
    if (ev.cost > 0) bits.push(`$${ev.cost.toFixed(4)}`);
    if (ev.prompt_tokens > 0) bits.push(`${fmtK(ev.prompt_tokens)} → ${fmtK(ev.completion_tokens || 0)} tok`);
    /* "asked" is a turn that ended by putting questions to the user, which is
     * an ordinary ending rather than something that went wrong — the form is
     * right there in the transcript saying so. Listed with "stop" so it does
     * not get the badge that "cancelled" or "llm-error" deserve. */
    if (ev.stopped_by && ev.stopped_by !== "stop" && ev.stopped_by !== "asked") {
      bits.push(ev.stopped_by);
    }

    const content = [bits.join(" · ")];
    if (onInspect) {
      content.push(" · ");
      content.push(
        el(
          "button",
          {
            type: "button",
            class: "meta-link",
            title: "Open the Context tab: the exact request this conversation last sent",
            onClick: onInspect,
          },
          "Inspect ↗"
        )
      );
    }
    if (bits.length) meta(el("span", {}, ...content));
  },
};

/* Session totals, recomputed from the ledger rather than accumulated.
 *
 * A running `+=` total is only correct if every contribution arrives exactly
 * once, and on a socket that reconnects and replays, that is not a property the
 * UI can assume. Deriving the total from the rows means a duplicate frame, a
 * replay or a late arrival cannot corrupt it: the same rows always give the same
 * total, and it is right after a refresh because the rows come from the log.
 */
function recomputeTotals() {
  const cost = store.turnStats.reduce((sum, t) => sum + (t.cost || 0), 0);
  store.set({ spendSession: cost });
  store.touch("turnStats");
}

/* Renders one frame, in the parent's column or inside a sub-agent's block.
 *
 * A child's frames are the parent's frames in every respect but two, and both
 * are about not letting a child speak for the conversation:
 *
 *  - `busy` is the parent's. It drives the composer lock and the stop button,
 *    which act on the conversation, not on a child. A child that outlives its
 *    parent's turn would otherwise leave the composer locked with nothing the
 *    user could stop.
 *  - a turn ledger row is keyed by sequence, and a child numbers its own log
 *    from one, so its sequences collide with the parent's. Prefixing with the
 *    child id keeps the de-duplication honest while still counting the spend:
 *    a sub-agent's tokens are this conversation's tokens.
 */
function dispatch(ev) {
  if (!ev.agent) {
    RENDERERS[ev.kind]?.(ev);
    return;
  }

  // A child's own compaction card belongs in its block, not the parent's.
  const outerCompact = compactNode;
  compactNode = null;
  const entry = inAgent(ev, (child) => {
    switch (child.kind) {
      // A child's turn boundaries drive its own block's header, never the
      // conversation's busy state or ledger. Its streaming cursors still have
      // to be closed off, or a second turn in the same child would append to
      // the previous turn's half-finished bubble.
      case "turn-started":
        break;
      case "turn-finished":
        settleReasoning();
        live = null;
        break;
      default:
        RENDERERS[child.kind]?.(child);
    }
  });
  compactNode = outerCompact;

  if (ev.kind === "turn-finished") {
    settleAgent(entry, ev);
    const seq = `${ev.agent}:${ev.seq ?? 0}`;
    if (!store.turnStats.some((t) => t.seq === seq)) {
      store.turnStats.push({
        seq,
        agent: ev.agent_label || "sub-agent",
        cost: ev.cost || 0,
        prompt_tokens: ev.prompt_tokens || 0,
        completion_tokens: ev.completion_tokens || 0,
        iterations: ev.iterations || 1,
        stopped_by: ev.stopped_by || "stop",
        ts: ev.ts,
      });
      recomputeTotals();
    }
  }
}

export function applyEvent(ev) {
  const follow = atBottom();
  // Any event at all means this conversation is no longer empty, so the
  // placeholder goes before the first message is appended after it.
  root.querySelector(".empty")?.remove();
  dispatch(ev);
  if (follow) toBottom(false);
}

export function replay(events) {
  reset();
  if (!events.length) {
    showEmpty("No messages yet.", "Say something to get started.");
    return;
  }
  events.forEach(dispatch);
  recomputeTotals();
  // A restored transcript should start at the end, without animating there.
  toBottom(true);
}
