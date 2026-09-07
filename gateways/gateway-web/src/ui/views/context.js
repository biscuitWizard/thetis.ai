/* The Context tab: the conversation as the model actually receives it.
 *
 * Three views of one thing. Request is the exact post-repair JSON body this
 * conversation last sent to the provider — model, every message, the tool
 * definitions, the cache breakpoints — fetched over the `debug-request`
 * frame. It is the newest request, not the one belonging to any particular
 * turn: the store keeps one per conversation, so this says which and when
 * rather than implying per-turn history it does not have. Prompt is that
 * body's system message on its own, since "what does my system prompt look
 * like" is the question the raw array answers worst. Usage is what the turns
 * cost, accumulated from the turn-finished events already on the wire.
 */

import { el } from "../lib/dom.js";
import { store } from "../lib/store.js";
import * as rail from "./rail.js";
import { toast } from "../lib/toast.js";

let send = () => {};

const state = {
  segment: "request", // request | prompt | usage
  reply: null, // the last debug-request frame, ok or not
  loading: false,
};

export function mountContext(sendFn) {
  send = sendFn;
}

/** Tab activation: draw what is known, ask for what is not. */
export function openTab(segment) {
  if (segment) state.segment = segment;
  if (!store.current) {
    state.reply = null;
  }
  draw();
  refresh();
}

export function onFrame(frame) {
  if (frame.session && frame.session !== store.current) return;
  state.reply = frame;
  state.loading = false;
  if (rail.isOpen("context")) draw();
}

/** Called whenever the usage numbers move, to keep Usage live. */
export function onUsageChanged() {
  if (rail.isOpen("context") && state.segment === "usage") draw();
}

function refresh() {
  if (!store.current) return;
  state.loading = true;
  send({ type: "debug-request", id: store.current });
}

/** The conversation moved on, so the capture on screen is a turn behind.
 *  Only worth asking for while a view that shows it is open. */
export function onConversationAdvanced() {
  if (!rail.isOpen("context")) return;
  if (state.segment === "usage") return draw();
  refresh();
}

// --- drawing --------------------------------------------------------------------

export function draw() {
  const segments = [
    ["request", "Request"],
    ["prompt", "Prompt"],
    ["usage", "Usage"],
  ];
  const segmented = el(
    "div",
    { class: "segmented" },
    segments.map(([id, label]) =>
      el(
        "button",
        {
          type: "button",
          class: `segment${state.segment === id ? " is-active" : ""}`,
          onClick: () => {
            state.segment = id;
            draw();
          },
        },
        label
      )
    )
  );

  const blocks = [segmented];
  if (state.segment === "usage") blocks.push(...usageBlocks());
  else if (state.segment === "prompt") blocks.push(...promptBlocks());
  else blocks.push(...requestBlocks());

  rail.open({
    id: "context",
    title: "Context",
    subtitle: "what the model actually receives",
    blocks,
  });
}

/** The whole captured body, or why there is none. */
function requestBlocks() {
  const frame = state.reply;
  if (!store.current) return [note("Open a conversation to inspect its requests.")];
  if (!frame) return [note(state.loading ? "Asking the worker…" : "Nothing fetched yet.")];
  if (!frame.ok) return [note(frame.message), refreshButton()];

  const body = frame.body || {};
  const messages = Array.isArray(body.messages) ? body.messages : [];
  const tools = Array.isArray(body.tools) ? body.tools : [];

  const header = el(
    "div",
    { class: "ctx-meta" },
    el("span", { class: "mono" }, "POST /chat/completions"),
    el(
      "span",
      {},
      frame.ts_ms ? `most recent · sent ${new Date(frame.ts_ms).toLocaleString()}` : "most recent"
    ),
    el("span", { class: "ctx-note" }, "exact post-repair body · auth header excluded"),
    el(
      "div",
      { class: "ctx-meta-actions" },
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn",
          title: "Copy the whole request body as JSON",
          onClick: (event) => {
            const button = event.currentTarget;
            navigator.clipboard?.writeText(JSON.stringify(body, null, 2)).then(
              () => flash(button, "Copied"),
              () => toast("The browser refused clipboard access.", { tone: "error" })
            );
          },
        },
        "Copy JSON"
      ),
      refreshButton()
    )
  );

  const scalars = el(
    "div",
    { class: "ctx-scalars mono" },
    line("model", JSON.stringify(body.model ?? null)),
    line("stream", JSON.stringify(body.stream ?? null)),
    body.stream_options && line("stream_options", JSON.stringify(body.stream_options))
  );

  const messageRows = messages.map((message, index) => messageRow(message, index));

  return [
    header,
    scalars,
    section(`messages`, messages.length),
    ...messageRows,
    section("tools", tools.length),
    toolsRow(tools),
  ];
}

function line(key, value) {
  return el("div", { class: "ctx-line" }, el("span", { class: "ctx-key" }, `"${key}"`), ": ", value);
}

function section(title, count) {
  return el(
    "div",
    { class: "panel-section" },
    el(
      "div",
      { class: "panel-section-head" },
      el("h3", { class: "panel-section-title" }, title),
      el("span", { class: "panel-section-count" }, String(count))
    )
  );
}

/** One message in the array: role, a gist, its cache badge, the full text behind
 *  a disclosure. */
function messageRow(message, index) {
  const role = message.role || "?";
  const text = contentText(message.content);
  const cached = hasCacheControl(message);

  const gistBits = [];
  if (Array.isArray(message.tool_calls) && message.tool_calls.length) {
    const names = message.tool_calls
      .map((c) => c.function?.name || c.name || "?")
      .join(", ");
    gistBits.push(`${message.tool_calls.length} tool call${message.tool_calls.length === 1 ? "" : "s"} → ${names}`);
  }
  if (message.tool_call_id) gistBits.push(`for ${String(message.tool_call_id).slice(0, 18)}`);
  if (text) gistBits.push(text);

  const full = [];
  if (text) full.push(["content", text]);
  if (Array.isArray(message.tool_calls) && message.tool_calls.length) {
    full.push(["tool_calls", JSON.stringify(message.tool_calls, null, 2)]);
  }
  if (!full.length) full.push(["raw", JSON.stringify(message, null, 2)]);

  return el(
    "details",
    { class: "ctx-msg" },
    el(
      "summary",
      {},
      el("span", { class: "ctx-idx mono" }, String(index)),
      el("span", { class: `ctx-role is-${role}` }, role),
      el("span", { class: "ctx-gist" }, cut(gistBits.join(" · "), 90)),
      el("span", { class: "ctx-size mono" }, sizeOf(message)),
      cached ? el("span", { class: "pill pill-warn", title: "A prompt-cache breakpoint is attached here" }, "cache_control") : null
    ),
    ...full.map(([label, value]) => [
      el("div", { class: "tool-label" }, label),
      el("pre", { class: "ctx-pre" }, value),
    ])
  );
}

function toolsRow(tools) {
  if (!tools.length) return note("No tools in this request.");
  const names = tools
    .map((t) => t.function?.name || t.name || "?")
    .join("  ·  ");
  return el(
    "details",
    { class: "ctx-msg" },
    el(
      "summary",
      {},
      el("span", { class: "ctx-role is-tools" }, "function definitions"),
      el("span", { class: "ctx-gist" }, cut(names, 90)),
      el("span", { class: "ctx-size mono" }, sizeOf(tools))
    ),
    el("div", { class: "tool-label" }, "names"),
    el("pre", { class: "ctx-pre" }, names),
    el("div", { class: "tool-label" }, "full json"),
    el("pre", { class: "ctx-pre" }, JSON.stringify(tools, null, 2))
  );
}

/** The Prompt segment: the system message, readable on its own. */
function promptBlocks() {
  const frame = state.reply;
  if (!store.current) return [note("Open a conversation to inspect its prompt.")];
  if (!frame) return [note(state.loading ? "Asking the worker…" : "Nothing fetched yet.")];
  if (!frame.ok) return [note(frame.message), refreshButton()];

  const messages = Array.isArray(frame.body?.messages) ? frame.body.messages : [];
  const system = messages.find((m) => m.role === "system");
  if (!system) return [note("This request carries no system message."), refreshButton()];

  const text = contentText(system.content);
  return [
    el(
      "div",
      { class: "ctx-meta" },
      el(
        "span",
        {},
        `the most recent request's system message · ~${text.length.toLocaleString()} chars`
      ),
      el("div", { class: "ctx-meta-actions" }, refreshButton())
    ),
    el("pre", { class: "ctx-prompt" }, text),
  ];
}

/** The Usage segment: session totals and the per-turn ledger. */
function usageBlocks() {
  const stats = store.turnStats || [];
  const live = store.liveTurn;
  const total = stats.reduce(
    (acc, t) => ({
      cost: acc.cost + (t.cost || 0),
      prompt: acc.prompt + (t.prompt_tokens || 0),
      completion: acc.completion + (t.completion_tokens || 0),
    }),
    { cost: 0, prompt: 0, completion: 0 }
  );

  // The turn in flight counts toward the totals as it spends, so the figures
  // move while a long turn runs rather than jumping when it ends.
  const blocks = [
    el(
      "div",
      { class: "ctx-totals" },
      stat("session cost", `$${(total.cost + (live?.cost || 0)).toFixed(4)}`),
      stat("prompt tokens", (total.prompt + (live?.prompt || 0)).toLocaleString()),
      stat("completion", (total.completion + (live?.completion || 0)).toLocaleString()),
      stat("turns", String(stats.length + (live ? 1 : 0)))
    ),
  ];

  if (live) {
    blocks.push(
      el(
        "div",
        { class: "ctx-turn is-live mono" },
        el("span", { class: "ctx-turn-n" }, store.busy ? "running" : "stopped"),
        el("span", {}, `$${live.cost.toFixed(4)}`),
        el("span", {}, `${fmtK(live.prompt)} → ${fmtK(live.completion)} tok`),
        el("span", {}, `${live.steps} step${live.steps === 1 ? "" : "s"} so far`)
      )
    );
  }

  const last = store.lastUsage;
  if (last?.prompt) {
    const share = last.cached > 0 ? ` · ${Math.round((last.cached / last.prompt) * 100)}% cached` : "";
    blocks.push(note(`last call: ${fmtK(last.prompt)} → ${fmtK(last.completion || 0)} tokens${share}${store.lastModel ? ` · ${store.lastModel}` : ""}`));
  }

  if (!stats.length) {
    if (!live) blocks.push(note("Nothing spent in this conversation yet."));
    return blocks;
  }

  blocks.push(section("per turn, newest first", stats.length));
  blocks.push(
    ...stats
      .slice()
      .reverse()
      .map((t, i) =>
        el(
          "div",
          { class: "ctx-turn mono" },
          el("span", { class: "ctx-turn-n" }, `#${stats.length - i}`),
          el("span", {}, `$${(t.cost || 0).toFixed(4)}`),
          el("span", {}, `${fmtK(t.prompt_tokens || 0)} → ${fmtK(t.completion_tokens || 0)} tok`),
          el("span", {}, t.iterations > 1 ? `${t.iterations} steps` : "1 step"),
          t.stopped_by && t.stopped_by !== "stop" ? el("span", { class: "ctx-stop" }, t.stopped_by) : null
        )
      )
  );
  return blocks;
}

function stat(label, value) {
  return el(
    "div",
    { class: "ctx-stat" },
    el("div", { class: "ctx-stat-value mono" }, value),
    el("div", { class: "ctx-stat-label" }, label)
  );
}

// --- small helpers ----------------------------------------------------------------

function refreshButton() {
  return el(
    "button",
    { type: "button", class: "ghost-btn", title: "Ask the worker again", onClick: () => { refresh(); } },
    "Refresh"
  );
}

function note(text) {
  return el("div", { class: "panel-note" }, text);
}

function flash(button, text) {
  const previous = button.textContent;
  button.textContent = text;
  setTimeout(() => (button.textContent = previous), 1200);
}

/** Plain text of an OpenAI-style content field (string, or blocks). */
function contentText(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((block) => (typeof block === "string" ? block : block?.text || `[${block?.type || "block"}]`))
      .join("\n");
  }
  return content == null ? "" : JSON.stringify(content);
}

function hasCacheControl(message) {
  if (message.cache_control) return true;
  if (Array.isArray(message.content)) {
    return message.content.some((block) => block && typeof block === "object" && block.cache_control);
  }
  return false;
}

function sizeOf(value) {
  const bytes = JSON.stringify(value)?.length || 0;
  if (bytes < 1024) return `${bytes} B`;
  return `${(bytes / 1024).toFixed(bytes < 10240 ? 1 : 0)} KB`;
}

function cut(text, max) {
  const flat = String(text ?? "").replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

function fmtK(n) {
  return n >= 10000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}
