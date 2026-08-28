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

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";
import { renderMarkdown } from "../lib/markdown.js";

let root = null;
let live = null; // element collecting streamed tokens
let onInspect = null; // opens the Context tab at the latest request
let pendingNode = null; // the optimistic row for a message not yet acknowledged
// Tool calls awaiting their result, by call id. A call and its result are one
// row, so the result has to find the row the call opened.
const open = new Map();

/** Tool output longer than this starts folded behind "show all". */
const RESULT_PREVIEW_CHARS = 4000;

export function mountTranscript(hooks = {}) {
  root = $("transcript");
  onInspect = hooks.onInspect || null;
  showEmpty();
}

export function reset() {
  live = null;
  pendingNode = null;
  open.clear();
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
  return el(
    "div",
    { class: "thumbs" },
    attachments.map((a) =>
      a.mime?.startsWith("image/")
        ? el("img", { class: "thumb", src: `data:${a.mime};base64,${a.data}`, alt: a.name, title: a.name })
        : el("div", { class: "thumb-file" }, a.name)
    )
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

function row(role, who, ...content) {
  const node = el(
    "div",
    { class: `row ${role}` },
    el("div", { class: "row-head" }, who),
    ...content
  );
  root.append(node);
  return node;
}

function meta(content, tone = "") {
  root.append(el("div", { class: `meta ${tone}`.trim() }, content));
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
  root.append(node);
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
    root.append(node);
  }

  node.classList.remove("is-running");
  node.classList.toggle("is-bad", !ev.ok);
  const status = node.querySelector(".tool-status");
  if (status) status.textContent = ev.ok ? "ok" : "failed";
  node.append(...resultSection(ev.content));
}

function thumbs(attachments) {
  if (!attachments?.length) return null;
  return el(
    "div",
    { class: "thumbs" },
    attachments.map((a) =>
      a.data
        ? el("img", { class: "thumb", src: a.data, alt: a.name, title: a.name })
        : el("div", { class: "thumb-file" }, a.name)
    )
  );
}

function fmtK(n) {
  return n >= 10000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

// --- event renderers --------------------------------------------------------

const RENDERERS = {
  user(ev) {
    live = null;
    row("user", "you",
      ev.text ? el("div", { class: "bubble-text" }, ev.text) : null,
      thumbs(ev.attachments));
  },

  delta(ev) {
    if (!live) {
      const node = row("assistant", "thetis", el("div", { class: "bubble-text is-streaming" }));
      live = node.querySelector(".bubble-text");
    }
    live.textContent += ev.text;
  },

  assistant(ev) {
    // The final message is authoritative: it replaces whatever streamed (as
    // rendered markdown), so a reconnect part-way through still lands right.
    if (live) {
      live.classList.remove("is-streaming");
      live.classList.add("md");
      live.replaceChildren(...renderMarkdown(ev.text));
      live = null;
    } else if (ev.text?.trim()) {
      row("assistant", "thetis", el("div", { class: "bubble-text md" }, renderMarkdown(ev.text)));
    }

    // Each step reports its own usage, so the turn's cost can be watched as it
    // is spent instead of landing all at once when the turn ends.
    if (ev.usage) {
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
    open.set(ev.id, toolRow(ev));
  },

  "tool-result": completeToolRow,

  compacted(ev) {
    live = null;
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
    root.append(node);
  },

  nudge: (ev) => meta(`you interrupted: ${ev.text}`, "is-nudge"),
  note: (ev) => meta(ev.text),

  incident(ev) {
    live = null;
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

  "turn-started": () => store.set({ busy: true, liveTurn: null }),

  "turn-finished"(ev) {
    store.set({ busy: false });
    live = null;

    // The ledger the Usage view and the header's spend chip read. These totals
    // are authoritative for the turn, so the running tally it was accumulating
    // is dropped rather than added to them.
    store.turnStats.push({
      cost: ev.cost || 0,
      prompt_tokens: ev.prompt_tokens || 0,
      completion_tokens: ev.completion_tokens || 0,
      iterations: ev.iterations || 1,
      stopped_by: ev.stopped_by || "stop",
      ts: ev.ts,
    });
    store.set({
      spendSession: (store.spendSession || 0) + (ev.cost || 0),
      liveTurn: null,
    });
    store.touch("turnStats");

    const bits = [];
    if (ev.iterations > 1) bits.push(`${ev.iterations} steps`);
    if (ev.cost > 0) bits.push(`$${ev.cost.toFixed(4)}`);
    if (ev.prompt_tokens > 0) bits.push(`${fmtK(ev.prompt_tokens)} → ${fmtK(ev.completion_tokens || 0)} tok`);
    if (ev.stopped_by && ev.stopped_by !== "stop") bits.push(ev.stopped_by);

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

export function applyEvent(ev) {
  const follow = atBottom();
  // Any event at all means this conversation is no longer empty, so the
  // placeholder goes before the first message is appended after it.
  root.querySelector(".empty")?.remove();
  RENDERERS[ev.kind]?.(ev);
  if (follow) toBottom(false);
}

export function replay(events) {
  reset();
  if (!events.length) {
    showEmpty("No messages yet.", "Say something to get started.");
    return;
  }
  events.forEach((ev) => RENDERERS[ev.kind]?.(ev));
  store.touch("turnStats");
  // A restored transcript should start at the end, without animating there.
  toBottom(true);
}
