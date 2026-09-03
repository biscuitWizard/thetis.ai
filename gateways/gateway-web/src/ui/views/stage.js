/* The centre stage: a tab strip over the conversation.
 *
 * The first tab is always the conversation and cannot be closed. After it come
 * tabs the work itself opens:
 *
 *  - a sub-agent gets one when it spawns, and keeps it after it finishes. A
 *    finished child's tab is the record of what it did, and the whole reason to
 *    have one is to read that without hunting for a folded block.
 *  - a file gets one when it is double-clicked in the Files explorer, with an
 *    editor in it.
 *
 * The panes are all in the DOM at once and exactly one is visible, hidden
 * through the `hidden` attribute. That is deliberate: a sub-agent streaming into
 * a tab you are not looking at must keep streaming, and a half-typed file edit
 * is the one thing on this surface the host cannot give back.
 *
 * The composer belongs to the conversation, so it lives inside the chat pane
 * and goes away with it. A file tab shows Save / Revert in its place; a
 * sub-agent tab shows nothing — you cannot type at a child.
 *
 * Note on the house rule this breaks: the design skill used to say tabs over
 * the centre stage were tried and rejected, on the grounds that they make the
 * editor a peer of the conversation. That was overruled deliberately — see the
 * skill, which now records the reversal.
 */

import { $, clear, el, icon, setHidden } from "../lib/dom.js";
import { store } from "../lib/store.js";
import { toast } from "../lib/toast.js";

const CHAT = "chat";

let strip = null; // #stage-tabs
let stage = null; // #stage
let send = () => false;
let onRename = null;

/* Every tab, in strip order. The chat tab is index 0 and is never removed.
 * Each entry: { id, kind, label, pane, tabEl, closable, ...kind-specific } */
const tabs = [];
let active = CHAT;

/** Paths this module has asked the host to read, so it can tell its own
 *  `workspace-file` replies apart from the Files explorer's. */
const wanted = new Set();

/** Opens a sub-agent's inline block in the conversation and scrolls to it.
 *  Supplied by `mountStage`; see the note there about the import cycle. */
let revealInline = null;

function find(id) {
  return tabs.find((t) => t.id === id);
}

// --- the strip ----------------------------------------------------------------

const CLOSE_ICON = ["M6 6l8 8M14 6l-8 8"];

/* One button per tab, rebuilt on any change to the set.
 *
 * Rebuilt rather than patched because the strip is a dozen nodes at most and
 * the alternative is a diffing scheme that would be wrong in exactly the case
 * that matters: two children spawning in the same tick. */
function drawStrip() {
  clear(strip);
  for (const tab of tabs) {
    const isActive = tab.id === active;
    const label = el("span", { class: "stage-tab-label" }, tab.label || "untitled");

    const node = el(
      "button",
      {
        type: "button",
        class: `stage-tab${isActive ? " is-active" : ""} is-${tab.kind}${tab.dirty ? " is-dirty" : ""}`,
        title: tab.hint || tab.label || "",
        dataset: { tab: tab.id },
        "aria-current": isActive ? "true" : null,
        onClick: () => {
          // Clicking the conversation's own tab while it is already showing is
          // the rename affordance the chat header used to carry.
          if (isActive && tab.kind === CHAT) return beginRename(node, label);
          show(tab.id);
        },
      },
      tab.kind === "agent" ? el("span", { class: `stage-dot is-${tab.state || "running"}` }) : null,
      tab.kind === "file" ? el("span", { class: "stage-tab-icon" }, icon(["M5 3.5h7l3 3v10H5z", "M12 3.5v3h3"], { size: 13, width: 1.6 })) : null,
      label,
      tab.note ? el("span", { class: "stage-tab-note" }, tab.note) : null,
      tab.closable
        ? el(
            "span",
            {
              class: "stage-tab-close",
              role: "button",
              tabindex: "-1",
              title: `Close ${tab.label}`,
              onClick: (event) => {
                event.stopPropagation();
                close(tab.id);
              },
            },
            icon(CLOSE_ICON, { size: 12, width: 1.8 })
          )
        : null
    );
    strip.append(node);
    tab.tabEl = node;
  }
  revealActive();
}

/* Scrolls the active tab into view.
 *
 * Once enough children have spawned the strip scrolls rather than shrinking its
 * tabs, and the tab a click just activated can be off the right-hand edge —
 * focusAgent from the sidebar would then appear to do nothing. `nearest` keeps
 * the strip still when the tab is already visible, so ordinary switching does
 * not jitter. Guarded because linkedom has no scrollIntoView. */
function revealActive() {
  const node = tabs.find((tab) => tab.id === active)?.tabEl;
  if (node && typeof node.scrollIntoView === "function") {
    node.scrollIntoView({ block: "nearest", inline: "nearest" });
  }
}

/* Renames the conversation from its own tab.
 *
 * The input replaces the label in place, so the tab keeps its position and
 * width behaviour. Enter commits, Escape and blur cancel — a commit belongs to
 * a keypress, not to focus wandering off. Teardown is idempotent because Enter
 * restores and the blur that follows would otherwise restore a second time. */
function beginRename(node, label) {
  if (!store.current) {
    return toast("No conversation open — nothing to rename.", { tone: "error" });
  }
  const input = el("input", { class: "stage-title-input", type: "text", value: store.title || "" });
  label.replaceWith(input);
  input.focus();
  input.select();

  let restored = false;
  const restore = () => {
    if (restored) return;
    restored = true;
    input.replaceWith(label);
  };
  input.addEventListener("keydown", (event) => {
    event.stopPropagation();
    if (event.key === "Enter") {
      const next = input.value.trim();
      if (next && next !== store.title) onRename?.(next);
      restore();
    }
    if (event.key === "Escape") restore();
  });
  input.addEventListener("blur", restore);
  // The click that opened this would otherwise bubble into `show` again.
  node.blur();
}

// --- showing and closing ------------------------------------------------------

export function activeTab() {
  return active;
}

export function show(id) {
  const tab = find(id);
  if (!tab) return false;
  active = id;
  for (const other of tabs) setHidden(other.pane, other.id !== id);
  drawStrip();
  tab.onShow?.();
  return true;
}

function close(id) {
  const at = tabs.findIndex((t) => t.id === id);
  if (at <= 0) return; // the conversation's tab is not closable
  const [tab] = tabs.splice(at, 1);
  tab.pane.remove();
  if (active === id) {
    // Fall back to the neighbour on the left, which for a lone extra tab is
    // the conversation — where the user was before this tab existed.
    active = tabs[Math.max(0, at - 1)].id;
    for (const other of tabs) setHidden(other.pane, other.id !== active);
  }
  drawStrip();
}

function add(tab) {
  tab.pane = el("section", { class: "stage-pane", dataset: { pane: tab.id }, hidden: true });
  stage.append(tab.pane);
  tabs.push(tab);
  drawStrip();
  return tab;
}

// --- sub-agent tabs -----------------------------------------------------------

/* Opens a pane for a sub-agent and hands back the element its frames render
 * into. Called by the transcript on a child's first frame, so the pane exists
 * before anything is appended to it and the tab never misses the opening of the
 * stream.
 *
 * The body carries `.agent-body` on purpose: every rule that makes a child's
 * rows read as a child's rows — the avatar column dropped, the child's own
 * "user" row left-aligned because it is a brief and not something the reader
 * typed — already hangs off that class, and a second spelling of them would
 * drift from the inline block's. */
export function openAgentPane(id, label) {
  const tabId = `agent:${id}`;
  const found = find(tabId);
  if (found) return found.body;

  const body = el("div", { class: "agent-body is-stage" });
  const scroller = el("div", { class: "agent-stage" }, body);

  const tab = add({
    id: tabId,
    kind: "agent",
    agent: id,
    label: label || "sub-agent",
    hint: `Sub-agent ${label || id} — its own transcript`,
    state: "running",
    closable: true,
    body,
    scroller,
  });

  tab.pane.append(
    el(
      "div",
      { class: "stage-bar" },
      el("span", { class: `stage-dot is-running`, dataset: { role: "dot" } }),
      el("span", { class: "stage-bar-name" }, label || "sub-agent"),
      el("span", { class: "stage-bar-state", dataset: { role: "state" } }, "working"),
      el("span", { class: "stage-bar-gap" }),
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn",
          title: "Jump to this sub-agent's block in the conversation",
          // Delegated to the owner of the inline copy, which knows whether that
          // copy's rows have been built yet. Doing it here through the DOM
          // scrolled to a block that was still empty.
          onClick: () => {
            show(CHAT);
            revealInline?.(id);
          },
        },
        "Show in conversation"
      )
    ),
    scroller
  );
  return body;
}

/* Tab dots, labels and the finished-cost note follow `store.agents`, which the
 * transcript already publishes for the sidebar (rule 7: the number is in hand).
 * An entry disappearing from that list means the transcript was reset — a
 * conversation switch — so the tabs go with it, per the rule that sub-agent
 * tabs are per-conversation while file tabs are global. */
store.watch("agents", (list) => {
  if (!strip) return;
  const live = new Set((list || []).map((a) => `agent:${a.id}`));

  for (const tab of [...tabs]) {
    if (tab.kind === "agent" && !live.has(tab.id)) close(tab.id);
  }
  for (const agent of list || []) {
    const tab = find(`agent:${agent.id}`);
    if (!tab) continue;
    tab.label = agent.label || "sub-agent";
    tab.state = agent.state === "running" ? "running" : agent.state === "done" ? "done" : "bad";
    tab.note = agent.cost > 0 ? `$${agent.cost.toFixed(3)}` : "";

    const dot = tab.pane.querySelector('[data-role="dot"]');
    if (dot) dot.className = `stage-dot is-${tab.state}`;
    const state = tab.pane.querySelector('[data-role="state"]');
    if (state) state.textContent = agent.state === "running" ? "working" : agent.state;
  }
  drawStrip();
});

/* Registers work to do the first time a sub-agent's pane is actually shown.
 *
 * The transcript uses this to defer building the pane's rows: a child's tab may
 * never be opened, and a finished child has hundreds of rows that cost real time
 * to build. `show` fires it once, on the transition to visible, and then forgets
 * it — the callback is expected to be idempotent anyway, but a pane is only
 * filled once and re-running it on every switch would be a slow no-op.
 *
 * If the pane happens to be the active tab already, the callback runs now: the
 * pane is on screen and there would be no later `show` to trigger it. */
export function onAgentPaneShown(id, fn) {
  const tab = find(`agent:${id}`);
  if (!tab) return false;
  if (tab.id === active) {
    fn();
    return true;
  }
  tab.onShow = () => {
    tab.onShow = null;
    fn();
  };
  return true;
}

/** Brings a sub-agent's tab forward. Used by the sidebar's sub-agent rows. */
export function focusAgent(id) {
  return show(`agent:${id}`);
}

// --- file tabs ----------------------------------------------------------------

/* Opens a file in an editor tab, or brings its tab forward if it is already
 * open. The bytes come from the host over the same `workspace-*` frames the
 * Files explorer uses, so nothing here has a second idea of what a file is.
 *
 * The pane is built empty and filled when the read lands, rather than waiting
 * for the reply: the tab appearing is the acknowledgement that the double-click
 * did something, and a workspace read crosses a process boundary. */
export function openFile(path) {
  const tabId = `file:${path}`;
  const found = find(tabId);
  if (found) return show(tabId);

  const name = path.split("/").filter(Boolean).pop() || path;
  const tab = add({
    id: tabId,
    kind: "file",
    path,
    label: name,
    hint: path,
    closable: true,
    file: null,
    draft: "",
    dirty: false,
    status: null,
    editor: null,
    editable: false,
    // Whether the next read should overwrite the editor or merely confirm it —
    // see `onFile`.
    adopt: true,
  });
  buildFile(tab);
  show(tabId);
  read(tab, true);
  return true;
}

function read(tab, adopt) {
  tab.adopt = adopt;
  wanted.add(tab.path);
  send({ type: "workspace-read", path: tab.path });
}

/* Builds the fixed furniture of a file tab: a head, a body, a foot.
 *
 * The three stay put for the tab's whole life and only the body's contents and
 * the chrome's text are replaced. That is not tidiness — rebuilding the pane
 * around a live `<textarea>` destroys it, and with it the caret, the selection
 * and the scroll position. Saving a file did exactly that until it didn't. */
function buildFile(tab) {
  clear(tab.pane);

  tab.headState = el("span", { class: "stage-bar-state" }, "reading…");
  tab.download = el("span", { class: "stage-bar-gap" });
  const head = el(
    "div",
    { class: "stage-bar" },
    el("span", { class: "stage-bar-name" }, tab.path),
    tab.headState,
    el("span", { class: "stage-bar-gap" }),
    tab.download
  );

  tab.body = el("div", { class: "stage-body" }, el("p", { class: "panel-note" }, "Reading the file…"));

  tab.statusEl = el("span", { class: "file-status" }, "");
  tab.revertBtn = el(
    "button",
    { type: "button", class: "ghost-btn", hidden: true,
      title: "Throw the draft away and re-read the file from disk",
      onClick: () => revert(tab) },
    "Revert"
  );
  tab.saveBtn = el(
    "button",
    { type: "button", class: "ghost-btn is-primary", hidden: true,
      title: "Write this back to the workspace (⌘S)",
      onClick: () => save(tab) },
    "Save"
  );
  const foot = el("div", { class: "file-foot" }, tab.statusEl, el("span", { class: "stage-bar-gap" }), tab.revertBtn, tab.saveBtn);

  tab.pane.append(head, tab.body, foot);
}

/** Fills the body with an editor or a preview. Only called when the *shape*
 *  changes — a file arriving, or its kind changing under us. */
function drawBody(tab) {
  const file = tab.file;
  clear(tab.body);
  tab.editor = null;

  // A picture, a recording or a binary has no editor. The tab is still the
  // right place for it — it is what double-clicking the row asked for — so it
  // previews here rather than bouncing the user back to the rail.
  if (!tab.editable) {
    tab.body.classList.add("is-preview");
    tab.body.append(preview(file));
    tab.onShow = null;
    return;
  }

  tab.body.classList.remove("is-preview");
  const editor = el("textarea", { class: "file-editor", spellcheck: "false" });
  editor.value = tab.draft;
  editor.addEventListener("input", () => {
    tab.draft = editor.value;
    // A keystroke that undoes the last one puts the tab back to clean, so this
    // compares against the file rather than latching on first input.
    setDirty(tab, tab.draft !== (tab.file?.text ?? ""));
  });
  // Ctrl/Cmd+S is what a hand reaches for in a text box with a Save button.
  editor.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
      event.preventDefault();
      save(tab);
    }
  });
  tab.editor = editor;
  tab.body.append(editor);
  tab.onShow = () => editor.focus();
}

/* Everything about a file tab that is text rather than structure: the size and
 * mtime, the download link, the status line, and whether Save is offered. */
function syncFile(tab) {
  const file = tab.file;
  tab.headState.textContent = file
    ? [human(file.size), when(file.modified_ms)].filter(Boolean).join(" · ")
    : tab.unreadable
      ? "unreadable"
      : "reading…";

  clear(tab.download);
  if (file?.url) {
    tab.download.append(
      el(
        "a",
        { class: "ghost-btn", href: `${file.url}?download=1`, download: file.name, title: "Download this file" },
        "Download"
      )
    );
  }

  setHidden(tab.saveBtn, !tab.editable);
  setHidden(tab.revertBtn, !tab.editable);

  // An explicit status — saving, saved, or a refusal from the host — outranks
  // the standing dirty/clean note, which is what the line says the rest of the
  // time.
  const standing = tab.editable ? (tab.dirty ? "unsaved changes" : "saved") : "";
  tab.statusEl.textContent = tab.status ? tab.status.message : standing;
  tab.statusEl.classList.toggle("is-error", !!tab.status && !tab.status.ok);
}

function setDirty(tab, dirty) {
  if (dirty === tab.dirty) return;
  tab.dirty = dirty;
  // A draft in flight is no longer what the status line last said about it.
  if (dirty) tab.status = null;
  drawStrip();
  syncFile(tab);
}

function preview(file) {
  if (!file) return el("p", { class: "panel-note" }, "Reading the file…");
  if (file.kind === "image") return el("img", { class: "file-preview", src: file.url, alt: file.name });
  if (file.kind === "video") return el("video", { class: "file-preview", src: file.url, controls: true });
  if (file.kind === "audio") return el("audio", { class: "file-audio", src: file.url, controls: true });
  if (file.kind === "pdf") {
    return el(
      "p",
      { class: "panel-note" },
      el("a", { href: file.url, target: "_blank", rel: "noopener" }, `Open ${file.name} in a new tab`)
    );
  }
  return el(
    "p",
    { class: "panel-note" },
    "This file is binary (or too large to show inline). Download it to look inside."
  );
}

function save(tab) {
  if (!tab.file || !tab.editable) return;
  tab.status = { ok: true, message: "saving…" };
  syncFile(tab);
  send({ type: "workspace-write", path: tab.path, text: tab.draft });
  // Re-read rather than patching locally: the reply carries the new size and
  // mtime, which the head would otherwise go on misstating. `adopt: false`
  // because the editor is still the authority on its own text — see `onFile`.
  read(tab, false);
}

function revert(tab) {
  tab.status = null;
  read(tab, true);
}

// --- frames -------------------------------------------------------------------

/* A `workspace-file` reply. Adopted only for a path this module asked about:
 * the Files explorer reads files too, and a single click there must not stamp
 * over an editor tab's draft.
 *
 * `adopt` decides what the reply means for the text on screen. Opening and
 * Revert ask for the file *instead of* the draft, so the editor takes what
 * arrives. The re-read after a save only wants the new size and mtime — the
 * editor already holds what was written, and adopting there would reset the
 * caret to the top of the file on every ⌘S. If the disk disagrees with the
 * draft in that case, something else wrote the file, and the disk wins: the
 * alternative is an editor quietly holding a version that no longer exists. */
export function onFile(frame) {
  if (!wanted.has(frame.path)) return;
  wanted.delete(frame.path);
  const tab = find(`file:${frame.path}`);
  if (!tab) return;

  const kind = frame.kind;
  const editable =
    !!frame.text_available && kind !== "image" && kind !== "video" && kind !== "audio" && kind !== "pdf";
  const text = frame.text ?? "";
  const reshaped = editable !== tab.editable || !tab.file;
  tab.unreadable = false;
  const adopt = tab.adopt || !tab.editor || text !== tab.draft;

  tab.file = frame;
  tab.editable = editable;
  if (adopt) tab.draft = text;
  tab.dirty = tab.draft !== text;
  if (!tab.dirty && tab.status?.message === "saving…") tab.status = { ok: true, message: "saved" };

  if (reshaped) drawBody(tab);
  else if (adopt && tab.editor && tab.editor.value !== text) tab.editor.value = text;
  syncFile(tab);
  drawStrip();
  if (tab.id === active && reshaped) tab.onShow?.();
}

/** The outcome of a write this module fired. Successes show up in the re-read
 *  that follows, so this is really here to carry a refusal — a permission
 *  error, a path outside the workspace — into the tab that caused it. */
export function onResult(frame) {
  if (frame.op !== "write" && frame.op !== "read") return;
  const tab = find(`file:${frame.path}`);
  if (!tab) return;

  // A read that failed never produces a `workspace-file`, so without this the
  // tab would sit on "Reading the file…" for good. Drop the claim on the path
  // too, or a later reply for it would be taken for this one's.
  if (frame.op === "read") {
    if (frame.ok) return;
    wanted.delete(frame.path);
    tab.status = { ok: false, message: frame.message };
    tab.unreadable = true;
    syncFile(tab);
    return;
  }

  tab.status = frame.ok ? { ok: true, message: "saved" } : { ok: false, message: frame.message };
  syncFile(tab);
}

// --- odds and ends ------------------------------------------------------------

function human(n) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = Number(n) || 0;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return unit === 0 ? `${value} B` : `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

function when(ms) {
  return ms ? new Date(ms).toLocaleString() : "";
}

/** The conversation's tab carries its title, so renaming and the host's own
 *  naming of a conversation from its first message both land here. */
export function setChatTitle(title) {
  const tab = find(CHAT);
  if (!tab) return;
  tab.label = title || "—";
  tab.hint = title ? `${title} — click again to rename` : "Click again to rename";
  drawStrip();
}

/** Called once from app.js, before anything that can open a tab. */
export function mountStage(hooks = {}) {
  strip = $("stage-tabs");
  stage = $("stage");
  send = hooks.sendFrame || (() => false);
  onRename = hooks.onRename || null;
  // Injected rather than imported: the transcript imports this module to get its
  // panes, so reaching back into it directly would be a cycle. The owner of both
  // (app.js) supplies the one call that crosses.
  revealInline = hooks.onRevealInline || null;

  tabs.length = 0;
  tabs.push({
    id: CHAT,
    kind: CHAT,
    label: "—",
    hint: "Click again to rename",
    pane: $("pane-chat"),
    closable: false,
  });
  active = CHAT;
  setHidden($("pane-chat"), false);
  drawStrip();

  return { show, openFile, focusAgent, openAgentPane, setChatTitle, activeTab };
}
