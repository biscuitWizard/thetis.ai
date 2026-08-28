/* The terminal drawer.
 *
 * The agent opens long-lived shells and runs builds in them, and until now the
 * only trace of that in the browser was a tool-result block after the fact.
 * This shows them live: a dock along the foot of the conversation column, one
 * tab per shell, animating up when the first one opens and shrinking the
 * transcript rather than covering it.
 *
 * Deliberately *not* a rail tab. Every inspector is a rail tab because an
 * inspector is something you consult; a terminal is something you watch while
 * the agent works, in parallel with reading the transcript. The rail is one
 * panel at a time, so putting it there would mean choosing between watching the
 * build and watching the branch graph.
 *
 * Rendering is xterm.js, vendored under /vendor. Shell output is a real
 * terminal protocol — carriage returns rewriting a progress line, cursor moves,
 * 256-colour SGR, wide characters — and every hand-rolled attempt at it turns
 * `cargo build` into a screenful of escape soup. The library is loaded lazily on
 * the first terminal that appears, so a conversation that never opens a shell
 * never pays for 277KB of emulator.
 *
 * Read-only: there is no host path for writing to a shell's stdin from a
 * browser, and a terminal that swallows keystrokes silently would be worse than
 * one that says it is a view.
 */

import { $, clear, el, icon } from "../lib/dom.js";
import { toast } from "../lib/toast.js";

// Height of the drawer, remembered across conversations and reloads. In
// localStorage rather than the store: it is a property of this screen, not of
// the conversation, and following you between conversations is the point.
const HEIGHT_KEY = "thetis.terminal.height";
const DEFAULT_H = 300;
const MIN_H = 140;
const maxH = () => Math.round(window.innerHeight * 0.72);

// Output arriving for a tab whose emulator does not exist yet is held here.
// Capped: a `cargo build` in a background tab would otherwise grow without
// bound, and only the tail is ever worth showing.
const PENDING_CAP = 256 * 1024;

const ICONS = {
  chevron: "M6 8l4 4 4-4",
  close: "M6 6l8 8M14 6l-8 8",
  trash: "M4 6h12M8 6V4.5h4V6M6 6v9.5h8V6M8.5 9v4M11.5 9v4",
};

let dock = null;
let tabsEl = null;
let bodyEl = null;
let footEl = null;
let chipEl = null;

/** Session the drawer is showing. The drawer only ever holds one
 *  conversation's shells: an emulator per shell per conversation would be a
 *  megabyte of retained canvas for conversations nobody is looking at. */
let sessionId = null;
/** id -> {id, cwd, shell, alive, commands, busy, pending, term, fit, pane, unread}
 * `unread` means "activity since you last looked at this tab", and only ever
 * decorates a tab that is not the active one. */
const tabs = new Map();
let activeId = null;
let openState = false;
let collapsed = false;
let libPromise = null;
let resizeObserver = null;

let requestList = () => {};

// --- the vendored library ---------------------------------------------------

/* Loaded with a classic script tag rather than `import`, because xterm ships a
 * UMD bundle: it has no export map, and in a module context its
 * `typeof exports` dance ends up assigning onto `self`. Fetching it as a module
 * would give a syntax-free file that defines nothing. */
/* Resolved against this module's own URL, never written as `/vendor/...`.
 * The absolute form 404s under `/preview/<session>/`, which is the one place a
 * UI change is looked at before it reaches trunk: the host rewrites asset
 * references in the HTML it serves, but a URL this file builds at runtime is
 * invisible to that. Relative means the preview and the live port both work. */
const asset = (name) => new URL(`../vendor/${name}`, import.meta.url).href;

function loadLib() {
  if (libPromise) return libPromise;
  libPromise = new Promise((resolve, reject) => {
    if (!document.querySelector("link[data-xterm]")) {
      document.head.append(
        el("link", { rel: "stylesheet", href: asset("xterm.css"), "data-xterm": true })
      );
    }
    const script = el("script", { src: asset("xterm.js"), "data-xterm": true });
    script.addEventListener("load", () => {
      const addon = el("script", { src: asset("xterm-addon-fit.js"), "data-xterm": true });
      // The fit addon is a nicety — without it the emulator still renders, just
      // at its default 80 columns — so a failure to load it resolves rather
      // than taking the whole drawer down with it.
      addon.addEventListener("load", () => resolve(true));
      addon.addEventListener("error", () => resolve(true));
      document.head.append(addon);
    });
    script.addEventListener("error", () => reject(new Error("xterm.js did not load")));
    document.head.append(script);
  });
  return libPromise;
}

/** The UMD bundles land in one of two shapes depending on the loader they
 *  detect; take whichever is there rather than guessing. */
function constructors() {
  const Term = window.Terminal || window.xterm?.Terminal;
  const Fit = window.FitAddon?.FitAddon || window.FitAddon;
  return { Term, Fit: typeof Fit === "function" ? Fit : null };
}

/* The palette comes from theme.css, read back through the cascade: xterm wants
 * colours as strings and cannot resolve a custom property itself. Every
 * --term-* token is a plain hex literal for exactly this reason — a
 * color-mix() would arrive here unresolved and be rejected. */
function themeColors() {
  const css = getComputedStyle(document.documentElement);
  const tok = (name, fallback) => css.getPropertyValue(name).trim() || fallback;
  return {
    background: tok("--term-bg", "#0c0c12"),
    foreground: tok("--term-fg", "#d7d7e3"),
    cursor: tok("--term-cursor", "#7c9cff"),
    cursorAccent: tok("--term-bg", "#0c0c12"),
    selectionBackground: tok("--term-selection", "#2c3560"),
    black: tok("--term-black", "#2b2b38"),
    red: tok("--term-red", "#f2788f"),
    green: tok("--term-green", "#7fd18f"),
    yellow: tok("--term-yellow", "#e8b673"),
    blue: tok("--term-blue", "#7c9cff"),
    magenta: tok("--term-magenta", "#c69bf0"),
    cyan: tok("--term-cyan", "#74ccd4"),
    white: tok("--term-white", "#c6c6d4"),
    brightBlack: tok("--term-bright-black", "#4e4e62"),
    brightRed: tok("--term-bright-red", "#ff93a7"),
    brightGreen: tok("--term-bright-green", "#9ce0a9"),
    brightYellow: tok("--term-bright-yellow", "#f5cd93"),
    brightBlue: tok("--term-bright-blue", "#9db4ff"),
    brightMagenta: tok("--term-bright-magenta", "#d9b6ff"),
    brightCyan: tok("--term-bright-cyan", "#96e2e9"),
    brightWhite: tok("--term-bright-white", "#f0f0f6"),
  };
}

// --- mounting ---------------------------------------------------------------

export function mountTerminals({ onRequest }) {
  requestList = onRequest || (() => {});
  dock = $("terminal-dock");
  chipEl = $("chip-terminals");
  if (!dock) return api;

  const grip = el("div", {
    class: "term-grip",
    role: "separator",
    "aria-orientation": "horizontal",
    title: "Drag to resize the terminal",
    onpointerdown: beginDrag,
  });

  tabsEl = el("nav", { class: "term-tabs", "aria-label": "Terminal sessions" });
  bodyEl = el("div", { class: "term-body" });
  footEl = el("div", { class: "term-foot" });

  const head = el(
    "header",
    { class: "term-head" },
    tabsEl,
    el(
      "div",
      { class: "term-head-actions" },
      el(
        "button",
        {
          class: "icon-btn sm",
          title: "Clear this terminal's view (the shell keeps running)",
          "aria-label": "Clear this terminal's view",
          onclick: clearActive,
        },
        icon(ICONS.trash, { size: 14 })
      ),
      el(
        "button",
        {
          class: "icon-btn sm term-collapse",
          title: "Collapse to the tab strip",
          "aria-label": "Collapse the terminal",
          onclick: () => setCollapsed(!collapsed),
        },
        icon(ICONS.chevron, { size: 14 })
      ),
      el(
        "button",
        {
          class: "icon-btn sm",
          title: "Hide the terminal drawer — the shells keep running",
          "aria-label": "Hide the terminal drawer",
          onclick: () => setOpen(false),
        },
        icon(ICONS.close, { size: 14 })
      )
    )
  );

  clear(dock).append(grip, head, bodyEl, footEl);
  dock.style.height = `${storedHeight()}px`;
  watchDockTransitions();

  // One observer for the drawer, not one per emulator: fit is only ever needed
  // for whichever tab is on screen, and a resize during the open animation
  // fires this every frame already.
  if (window.ResizeObserver) {
    resizeObserver = new ResizeObserver(scheduleFit);
    resizeObserver.observe(bodyEl);
  }
  window.addEventListener("resize", scheduleFit);

  if (chipEl) {
    chipEl.addEventListener("click", () => {
      if (!tabs.size) {
        return toast("No terminals open in this conversation yet.", { tone: "info" });
      }
      setOpen(!openState);
      if (openState && collapsed) setCollapsed(false);
    });
  }

  drawChip();
  return api;
}

// --- the drawer itself ------------------------------------------------------

function storedHeight() {
  const raw = Number(localStorage.getItem(HEIGHT_KEY));
  if (!Number.isFinite(raw) || raw <= 0) return DEFAULT_H;
  return Math.min(Math.max(raw, MIN_H), maxH());
}

/* Open and close animate the drawer's height, which means the element must stay
 * in the layout while it moves — so `hidden` goes on only once the transition
 * has finished, and comes off a frame before it starts. Anything that reads
 * "is it open?" reads `openState`, not the attribute. */
function setOpen(open) {
  if (open === openState) return;
  openState = open;

  if (open) {
    dock.hidden = false;
    dock.style.height = "0px";
    // Two frames: one for `hidden` to stop suppressing layout, one for the
    // browser to have a starting height to animate from. Collapsing these into
    // one makes the drawer appear at full height with no motion.
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        dock.classList.add("is-open");
        dock.style.height = `${collapsed ? 0 : storedHeight()}px`;
        showTab(activeId);
      })
    );
  } else {
    dock.classList.remove("is-open");
    dock.style.height = "0px";
  }
  drawChip();
}

/* `hidden` goes back on only once the closing animation has finished — set
 * during the transition it would snap the drawer out of the layout instead of
 * letting the transcript grow back into the space. */
function watchDockTransitions() {
  dock.addEventListener("transitionend", (event) => {
    if (event.target !== dock || event.propertyName !== "height") return;
    if (!openState) dock.hidden = true;
    scheduleFit();
  });
}

function setCollapsed(next) {
  collapsed = next;
  dock.classList.toggle("is-collapsed", collapsed);
  dock.style.height = collapsed ? "" : `${storedHeight()}px`;
  scheduleFit();
}

function beginDrag(event) {
  if (collapsed) return;
  event.preventDefault();
  const startY = event.clientY;
  const startH = dock.getBoundingClientRect().height;
  dock.classList.add("is-dragging");

  const move = (e) => {
    const next = Math.min(Math.max(startH + (startY - e.clientY), MIN_H), maxH());
    dock.style.height = `${next}px`;
  };
  const done = () => {
    dock.classList.remove("is-dragging");
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", done);
    localStorage.setItem(HEIGHT_KEY, String(Math.round(dock.getBoundingClientRect().height)));
    scheduleFit();
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", done);
}

let fitTimer = null;
function scheduleFit() {
  cancelAnimationFrame(fitTimer);
  fitTimer = requestAnimationFrame(() => {
    const tab = tabs.get(activeId);
    if (!tab?.fit || !tab.pane?.isConnected || collapsed || !openState) return;
    try {
      tab.fit.fit();
    } catch {
      // A fit against a zero-height container throws rather than doing
      // nothing; mid-animation that is expected and not worth reporting.
    }
  });
}

// --- tabs -------------------------------------------------------------------

function drawTabs() {
  clear(tabsEl);
  const ordered = [...tabs.values()].sort((a, b) => collate(a.id, b.id));
  for (const tab of ordered) {
    const dot = tab.busy
      ? "term-dot is-busy"
      : tab.alive
        ? "term-dot is-ok"
        : "term-dot is-done";
    tabsEl.append(
      el(
        "button",
        {
          class: `term-tab${tab.id === activeId ? " is-active" : ""}${tab.unread ? " has-activity" : ""}`,
          title: `${tab.id} — ${tab.shell || "shell"} in ${tab.cwd || "?"}${
            tab.remote ? ` on ${tab.remote}` : ""
          }`,
          onclick: () => showTab(tab.id),
        },
        el("span", { class: dot }),
        el("span", { class: "term-tab-label mono" }, tab.id),
        // Which machine, on the tab itself. A command that ran somewhere else
        // is the thing you most need not to misread.
        tab.remote && el("span", { class: "term-tab-host" }, tab.remote),
        !tab.alive && el("span", { class: "term-tab-note" }, "exited")
      )
    );
  }
  if (!tabs.size) {
    tabsEl.append(
      el("span", { class: "term-empty" }, "No shells open — the agent opens one when it needs to run something.")
    );
  }
}

/** Sorts `term-2` before `term-10`, which a plain string sort gets backwards. */
function collate(a, b) {
  return a.localeCompare(b, undefined, { numeric: true });
}

function drawFoot() {
  const tab = tabs.get(activeId);
  clear(footEl);
  if (!tab) return;
  footEl.append(
    el("span", { class: "term-cwd mono", title: tab.cwd }, tab.cwd || ""),
    el(
      "span",
      { class: "term-meta" },
      `${tab.remote ? `${tab.remote} · ` : ""}${tab.shell || "shell"} · ${tab.commands} command${
        tab.commands === 1 ? "" : "s"
      } · view only`
    )
  );
}

function showTab(id) {
  const tab = tabs.get(id) || [...tabs.values()][0];
  if (!tab) {
    activeId = null;
    drawTabs();
    drawFoot();
    return;
  }
  activeId = tab.id;
  tab.unread = false;
  drawTabs();
  drawFoot();

  clear(bodyEl);
  ensureEmulator(tab)
    .then(() => {
      if (activeId !== tab.id) return;
      clear(bodyEl).append(tab.pane);
      // The instance may have been created while detached, in which case xterm
      // has never measured a cell; opening it here is what gives it a size.
      if (!tab.opened) {
        tab.term.open(tab.pane);
        tab.opened = true;
      }
      flush(tab);
      scheduleFit();
      tab.term.scrollToBottom();
    })
    .catch((e) => {
      clear(bodyEl).append(
        el(
          "div",
          { class: "term-fallback" },
          el("p", {}, "The terminal renderer did not load."),
          el("pre", { class: "mono" }, tab.pending.join("")),
          el("p", { class: "term-fallback-note" }, String(e.message || e))
        )
      );
    });
}

async function ensureEmulator(tab) {
  if (tab.term) return tab;
  await loadLib();
  const { Term, Fit } = constructors();
  if (!Term) throw new Error("xterm.js loaded but defined no Terminal");

  tab.pane = el("div", { class: "term-pane" });
  tab.term = new Term({
    // The host sends bare "\n"; a real terminal would see "\r\n", and without
    // this every line starts where the last one ended and the output walks off
    // to the right.
    convertEol: true,
    disableStdin: true,
    cursorBlink: false,
    scrollback: 5000,
    fontFamily: getComputedStyle(document.documentElement).getPropertyValue("--mono").trim(),
    fontSize: 12,
    lineHeight: 1.25,
    theme: themeColors(),
  });
  if (Fit) {
    tab.fit = new Fit();
    tab.term.loadAddon(tab.fit);
  }
  return tab;
}

/** Moves anything buffered while the emulator did not exist into it. */
function flush(tab) {
  if (!tab.term || !tab.pending.length) return;
  const text = tab.pending.join("");
  tab.pending.length = 0;
  tab.term.write(text);
}

function write(tab, text) {
  if (!text) return;
  if (tab.term && tab.opened) {
    tab.term.write(text);
    return;
  }
  tab.pending.push(text);
  let total = tab.pending.reduce((n, s) => n + s.length, 0);
  while (total > PENDING_CAP && tab.pending.length > 1) {
    total -= tab.pending.shift().length;
  }
}

function clearActive() {
  const tab = tabs.get(activeId);
  if (!tab) return toast("No terminal selected.", { tone: "info" });
  tab.pending.length = 0;
  tab.term?.clear();
}

function drawChip() {
  if (!chipEl) return;
  const live = [...tabs.values()].filter((t) => t.alive).length;
  chipEl.hidden = !sessionId || tabs.size === 0;
  if (chipEl.hidden) return;
  const busy = [...tabs.values()].some((t) => t.busy);
  chipEl.classList.toggle("is-busy", busy);
  chipEl.classList.toggle("is-on", openState);
  clear(chipEl).append(
    el("span", { class: `term-dot ${busy ? "is-busy" : live ? "is-ok" : "is-done"}` }),
    el("span", {}, `${tabs.size} terminal${tabs.size === 1 ? "" : "s"}`)
  );
  chipEl.title = openState
    ? "Hide the terminal drawer"
    : "Show the shells this conversation has open";
}

function upsert(info) {
  let tab = tabs.get(info.id);
  if (!tab) {
    tab = {
      id: info.id,
      cwd: "",
      shell: "",
      remote: "",
      alive: true,
      commands: 0,
      busy: false,
      unread: false,
      pending: [],
      term: null,
      fit: null,
      pane: null,
      opened: false,
    };
    tabs.set(info.id, tab);
  }
  if (info.cwd) tab.cwd = info.cwd;
  if (info.shell) tab.shell = info.shell;
  // Empty on an output event, which restates none of the three — so only ever
  // written when the host actually said something.
  if (info.remote) tab.remote = info.remote;
  if (typeof info.alive === "boolean") tab.alive = info.alive;
  if (typeof info.commands === "number") tab.commands = info.commands;
  return tab;
}

function dispose() {
  for (const tab of tabs.values()) {
    try {
      tab.term?.dispose();
    } catch {
      // Disposing twice, or disposing one that never opened, is harmless here
      // and must not stop the rest of the switch.
    }
  }
  tabs.clear();
  activeId = null;
  if (bodyEl) clear(bodyEl);
}

// --- the wire ---------------------------------------------------------------

export const api = {
  /** Switching conversations: the drawer belongs to the one on screen. */
  setSession(id) {
    if (id === sessionId) return;
    sessionId = id;
    dispose();
    // Closed, not merely emptied: a drawer left standing over another
    // conversation's shells is a lie, and the reply below reopens it if this
    // conversation has any.
    setOpen(false);
    drawTabs();
    drawFoot();
    drawChip();
    if (id) requestList(id);
  },

  /** The full picture, in answer to a `terminals` request. */
  onList(frame) {
    if (frame.session !== sessionId) return;
    if (!frame.ok) return;
    const seen = new Set();
    for (const view of frame.terminals || []) {
      const tab = upsert(view);
      seen.add(view.id);
      // Replace rather than append: this is the authoritative transcript, and
      // anything already buffered is a prefix of it.
      if (view.transcript) {
        tab.pending = [view.transcript];
        if (tab.term && tab.opened) {
          tab.term.clear();
          tab.term.write(view.transcript);
        }
      }
    }
    // A shell reaped while this tab was elsewhere.
    for (const id of [...tabs.keys()]) {
      if (!seen.has(id)) {
        tabs.get(id)?.term?.dispose();
        tabs.delete(id);
      }
    }
    if (!tabs.has(activeId)) activeId = [...tabs.keys()].sort(collate)[0] || null;
    drawTabs();
    drawFoot();
    drawChip();
    if (tabs.size) {
      setOpen(true);
      if (openState) showTab(activeId);
    }
  },

  /** One piece of live shell activity. */
  onFeed(frame) {
    if (frame.session !== sessionId) return;
    if (!frame.id) return;
    const known = tabs.has(frame.id);
    const tab = upsert(frame);

    switch (frame.kind) {
      case "opened":
        tab.alive = true;
        // The point of the whole drawer: a shell appearing in the conversation
        // on screen brings it up, without a click.
        if (!openState) setOpen(true);
        // Only ever focus the *first* shell. An agent that opens a second one
        // while you are reading the first must not yank the view across —
        // its tab brightens to `has-activity` instead, and you go when ready.
        if (!activeId) activeId = frame.id;
        else if (frame.id !== activeId) tab.unread = true;
        break;
      case "command":
        tab.busy = true;
        tab.commands += 1;
        write(tab, frame.text);
        break;
      case "output":
        write(tab, frame.text);
        if (frame.id !== activeId) tab.unread = true;
        break;
      case "exit":
        tab.busy = false;
        break;
      case "closed":
        tab.alive = false;
        tab.busy = false;
        write(tab, "\n[session closed]\n");
        break;
    }

    if (frame.kind !== "output" || !known) {
      drawTabs();
      drawFoot();
      drawChip();
    }
    if (frame.kind === "opened" && openState) showTab(activeId);
    // A tab drawn as busy needs its dot to stop pulsing eventually, and the
    // foot's command count moves with every command.
    if (frame.kind === "exit") drawFoot();
  },

  isOpen() {
    return openState;
  },
};
