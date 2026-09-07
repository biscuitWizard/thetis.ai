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
 *  - the plan gets one as soon as the conversation has a plan. It is the only
 *    tab the agent opens by writing a document rather than by starting a
 *    process, and the only one with an action bar that starts a turn: Execute,
 *    with a mode and a model beside it.
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
import { renderMarkdown } from "../lib/markdown.js";
import { store } from "../lib/store.js";
import { toast } from "../lib/toast.js";
import { Picker } from "./picker.js";

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
      tab.kind === "plan" ? el("span", { class: "stage-tab-icon" }, icon(["M6 4.5h8M6 8h8M6 11.5h5", "M3.5 4.5h.01M3.5 8h.01M3.5 11.5h.01"], { size: 13, width: 1.6 })) : null,
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

// --- the plan tab -------------------------------------------------------------
//
// A plan is neither a transcript nor a file, and giving it a tab of its own is
// the point of this section. In the transcript a plan is a wall of prose that
// scrolls away under the investigation that produced it, and every revision
// appears as another copy — the reader has to work out which one is current. As
// a tab it is one document that changes in place, with the thing you actually
// want to do to a plan attached to it: hand it to an agent and watch it happen.
//
// The tab is read-only markdown, deliberately. Editing the plan is the agent's
// job through `plan_edit`, because a plan the user has silently rewritten is one
// the agent will keep editing against text that is no longer there. What the
// user gets instead is Execute, a mode, a model, and a note box — the levers
// that matter at the moment of approval.

const PLAN = "plan:doc";


/** Modes and models the Execute bar may offer, as the host last described them,
 *  along with what the user picked. Held here rather than in the tab so a redraw
 *  from a fresh `plan` frame does not lose the selection. */
const planChoice = { mode: "", model: "", modes: [], models: [] };

/** Opens the plan tab, or brings it forward. Called when a `plan` frame arrives
 *  with a body, and by the sidebar/keyboard route that asks for the plan. */
export function openPlan(focus = true) {
  let tab = find(PLAN);
  if (!tab) {
    tab = add({
      id: PLAN,
      kind: "plan",
      label: "Plan",
      hint: "The plan for this conversation",
      // Closable: a plan that has been executed is clutter until it is wanted
      // again, and the tab comes straight back from the sidebar. Closing it
      // never destroys the document — that lives in the session store.
      closable: true,
      plan: null,
    });
    buildPlan(tab);
  }
  if (focus) show(PLAN);
  return tab;
}

/* The furniture: an action bar, then the document.
 *
 * Same discipline as a file tab — built once, contents replaced — because the
 * note box is a live `<textarea>` the user may be halfway through typing when a
 * revision lands, and rebuilding the bar around it would take the caret with it.
 */
function buildPlan(tab) {
  clear(tab.pane);

  tab.planTitle = el("span", { class: "stage-bar-name" }, "Plan");
  tab.planRev = el("span", { class: "stage-bar-state" }, "");

  tab.modeMount = el("span", { class: "picker" });
  tab.modelMount = el("span", { class: "picker" });
  tab.execBtn = el(
    "button",
    {
      type: "button",
      class: "ghost-btn is-primary",
      title: "Switch this conversation into the chosen mode and start carrying the plan out",
      onClick: () => execute(tab),
    },
    "Execute"
  );

  tab.planBar = el(
    "div",
    { class: "stage-bar is-plan" },
    tab.planTitle,
    tab.planRev,
    el("span", { class: "stage-bar-gap" }),
    el("span", { class: "plan-exec" }, tab.modeMount, tab.modelMount, tab.execBtn)
  );

  tab.planBody = el("div", { class: "stage-body plan-body" }, el("p", { class: "panel-note" }, "Loading the plan…"));

  // A place to say something at the moment of approval — "skip step 3", "start
  // with the tests". It rides along with the execute message instead of becoming
  // a separate turn, because a note sent after the agent is already working
  // arrives as an interruption rather than as an instruction.
  tab.planNote = el("textarea", {
    class: "plan-note",
    rows: "2",
    spellcheck: "false",
    placeholder: "Anything to add before it starts (optional)",
  });
  tab.planStatus = el("span", { class: "file-status" }, "");

  tab.pane.append(
    tab.planBar,
    tab.planBody,
    el("div", { class: "plan-foot" }, tab.planNote, tab.planStatus)
  );

  // `drop: "down"` because this bar is at the top of the pane. The composer's
  // pickers open upward, which here would put the menu off the top of the
  // screen with none of its options clickable.
  tab.modePicker = new Picker(tab.modeMount, {
    drop: "down",
    title: "Which mode to execute in",
    options: () => planChoice.modes,
    selected: () => planChoice.mode,
    onSelect: (id) => {
      planChoice.mode = id;
      tab.modePicker.refresh();
    },
    render: (id) => planChoice.modes.find((m) => m.id === id)?.label || id || "mode",
  });
  tab.modelPicker = new Picker(tab.modelMount, {
    drop: "down",
    title: "Which model to execute with",
    options: () => [{ id: "", label: "Current model" }, ...planChoice.models],
    selected: () => planChoice.model,
    onSelect: (id) => {
      planChoice.model = id;
      tab.modelPicker.refresh();
    },
    render: (id) => planChoice.models.find((m) => m.id === id)?.label || "Current model",
  });
}

/* A `plan` frame: the document as the host holds it.
 *
 * Opens the tab when there is a plan and the tab is not there yet, so the plan
 * appearing is itself the notification — the same way a spawning sub-agent gets
 * a tab. It does not steal focus: the plan is usually written while the user is
 * reading the conversation, and yanking the view away mid-sentence is worse than
 * a dot on a tab. */
export function onPlan(frame) {
  planChoice.modes = frame.modes || [];
  planChoice.models = frame.models || [];
  if (!planChoice.mode || !planChoice.modes.some((m) => m.id === planChoice.mode)) {
    planChoice.mode = planChoice.modes[0]?.id || "";
  }
  if (planChoice.model && !planChoice.models.some((m) => m.id === planChoice.model)) {
    planChoice.model = "";
  }

  if (!frame.has_plan) {
    // The plan was never written, or this is a different conversation that has
    // none. An open tab is closed rather than left showing the last one's.
    if (find(PLAN)) close(PLAN);
    return;
  }

  const tab = openPlan(false);
  const fresh = tab.plan && tab.plan.revision !== frame.revision;
  tab.plan = frame;
  tab.label = frame.title?.trim() || "Plan";
  tab.hint = `${tab.label} — revision ${frame.revision}`;
  // A revision arriving while the user is looking elsewhere is worth a mark, the
  // same as a sub-agent's dot. Cleared when the tab is next shown.
  if (fresh && active !== PLAN) tab.note = "revised";
  drawPlan(tab);
  drawStrip();
}

function drawPlan(tab) {
  const plan = tab.plan;
  if (!plan) return;
  tab.planTitle.textContent = plan.title?.trim() || "Plan";
  tab.planRev.textContent = [
    `revision ${plan.revision}`,
    plan.updated_ms ? when(plan.updated_ms) : "",
  ]
    .filter(Boolean)
    .join(" · ");

  clear(tab.planBody);
  // `renderMarkdown` hands back an array of block nodes, so it spreads —
  // appending the array itself would stringify it into the pane.
  tab.planBody.append(...renderMarkdown(plan.body || ""));

  // No writable mode configured means Execute cannot do anything; saying so on
  // the disabled button beats a click that reports the problem afterwards.
  const ready = planChoice.modes.length > 0;
  tab.execBtn.disabled = !ready;
  if (!ready) tab.execBtn.title = "No writable mode is configured, so the plan cannot be executed";
  tab.modePicker.refresh();
  tab.modelPicker.refresh();

  tab.onShow = () => {
    if (!tab.note) return;
    tab.note = "";
    drawStrip();
  };
}

function execute(tab) {
  if (!tab.plan || !store.current) return;
  const note = tab.planNote.value.trim();
  send({
    type: "plan-execute",
    id: store.current,
    mode: planChoice.mode,
    model: planChoice.model || null,
    note,
  });
  tab.planNote.value = "";
  tab.planStatus.textContent = "handed to the agent — switching to the conversation";
  tab.planStatus.classList.remove("is-error");
  // The work shows up in the transcript, so that is where the user wants to be
  // the moment the button is pressed. Staying on a static document while a turn
  // begins elsewhere is the surest way to make the click feel like it failed.
  show(CHAT);
}

/** A refusal from `plan-execute` — no plan, or an unusable mode. */
export function onPlanError(message) {
  const tab = find(PLAN);
  if (!tab) return false;
  tab.planStatus.textContent = message;
  tab.planStatus.classList.add("is-error");
  return true;
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

  return { show, openFile, focusAgent, openAgentPane, setChatTitle, activeTab, openPlan };
}
