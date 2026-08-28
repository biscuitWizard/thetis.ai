/* The system status bar: one line across the foot of the whole shell.
 *
 * Everything here answers a question you should never have to click for —
 * which trunk am I looking at, is the UI I am being served the one trunk
 * describes, is anything building, and how much room is left on the machine.
 *
 * It is derived state with no event of its own, so it declares its own
 * invalidation: a slow poll while the tab is visible, plus an immediate
 * refresh on the events that can move any of these numbers. The poll stops
 * entirely when the tab is hidden — the frame costs a `git log -1` and one IPC
 * call per live worker, and a background tab should not pay it.
 */

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";

/** How often to re-ask while the tab is on screen. */
const POLL_MS = 10_000;

/** Events that move something on this bar, so it re-asks at once. */
export const REFRESHED_BY = [
  "modification",
  "branch-op",
  "turn-started",
  "turn-finished",
  "incident",
];

/** The word each state gets, and which semantic colour the dot takes. */
const STATE = {
  running: { label: "running", tone: "ok", hint: "Idle and healthy: nothing building, no turn in flight." },
  working: { label: "working", tone: "warn", hint: "A turn is running in at least one conversation." },
  building: { label: "rebuilding", tone: "warn", hint: "A component is compiling. Its old version stays loaded until the new one passes." },
  stale: { label: "UI stale", tone: "warn", hint: "The interface being served was built from an older trunk. Restarting the orchestrator picks up the current one." },
  degraded: { label: "fallback UI", tone: "err", hint: "No UI component is loaded, so the host-rendered fallback page is serving. A build has yet to land." },
};

let send = () => {};
let timer = null;
/** Set when the host answers `system-status` with "unknown frame type". */
let unsupported = false;

/** Mounts the bar and starts polling. `sender` submits a frame to the host. */
export function mountStatusbar(sender) {
  send = sender;
  draw();

  // Only poll a visible tab, and catch up the moment one comes back.
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stop();
    else {
      refresh();
      start();
    }
  });
  if (!document.hidden) start();
  refresh();

  return { refresh, onFrame };
}

function start() {
  stop();
  timer = setInterval(refresh, POLL_MS);
}

function stop() {
  clearInterval(timer);
  timer = null;
}

export function refresh() {
  if (unsupported) return;
  send({ type: "system-status" });
}

/* The running kernel predates this frame. Say so once, plainly, and stop
 * asking: an older host is a normal state here — the gateway keeps serving
 * trunk's UI across a kernel that has not restarted yet — and a bar that
 * silently showed nothing would look like a bug in the bar. */
export function onUnsupported() {
  if (unsupported) return;
  unsupported = true;
  stop();
  store.set({ system: null });
  const host = $("statusbar");
  if (!host) return;
  clear(host).append(
    el(
      "span",
      {
        class: "sb-item sb-quiet",
        title:
          "The running orchestrator does not answer system-status yet. " +
          "It arrives when this branch's kernel is built and restarted onto.",
      },
      "system status needs a newer orchestrator — restart once this branch's kernel is built"
    )
  );
}

/** The host's answer. Kept in the store so a redraw needs no round trip. */
export function onFrame(frame) {
  store.set({ system: frame });
  draw();
}

// --- rendering ---------------------------------------------------------------

function draw() {
  const host = $("statusbar");
  if (!host) return;
  const sys = store.system;

  if (!sys) {
    clear(host).append(
      el("span", { class: "sb-item sb-quiet" }, "asking the host for system status…")
    );
    return;
  }

  const state = STATE[sys.state] || {
    label: sys.state || "unknown",
    tone: "warn",
    hint: "The host reported a state this interface does not know about.",
  };

  const bits = [
    el(
      "span",
      { class: "sb-item", title: state.hint },
      el("span", { class: `sb-dot is-${state.tone}` }),
      el("span", { class: "sb-strong" }, state.label)
    ),
    trunkItem(sys),
    uiItem(sys),
    workersItem(sys),
    memoryItem(sys),
    loadItem(sys),
    el(
      "span",
      {
        class: "sb-item sb-right",
        title: `Thetis ${sys.version} · WIT contract ${sys.wit} · ${sys.sessions} conversation${
          sys.sessions === 1 ? "" : "s"
        } on record`,
      },
      el("span", { class: "sb-label" }, "thetis"),
      el("span", { class: "sb-mono" }, `v${sys.version}`),
      sys.uptime_s != null && el("span", { class: "sb-quiet" }, `up ${duration(sys.uptime_s)}`)
    ),
  ];

  clear(host).append(...bits.filter(Boolean));
}

/** Trunk: the version every conversation starts from, and this page came from. */
function trunkItem(sys) {
  const trunk = sys.trunk || {};
  const rev = (trunk.rev || "").slice(0, 12);
  if (!rev) {
    return el(
      "span",
      { class: "sb-item sb-quiet", title: "This checkout has no commits yet." },
      "no trunk commit"
    );
  }
  const detail = [
    trunk.subject ? `"${trunk.subject}"` : null,
    trunk.author || null,
    trunk.ts_ms ? ago(trunk.ts_ms) : null,
    trunk.dirty ? "the working tree has uncommitted changes" : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return el(
    "span",
    {
      class: "sb-item",
      title: `Trunk ${trunk.name} at ${rev} — ${detail}. What every new conversation starts from.`,
    },
    el("span", { class: "sb-label" }, "trunk"),
    el("span", { class: "sb-mono" }, `${trunk.name} ${rev}`),
    trunk.dirty && el("span", { class: "sb-flag is-warn" }, "dirty")
  );
}

/** Which UI build the loader is actually serving, against what trunk says. */
function uiItem(sys) {
  const ui = sys.ui || {};
  const revision = ui.revision != null ? ui.revision.toString(16).slice(0, 8) : "—";
  const tone = ui.serving === "current" ? null : ui.serving === "fallback" ? "err" : "warn";
  const hint = {
    current: "The interface you are looking at was built from trunk's current tree.",
    stale: "The loaded interface predates trunk's current tree. It updates on the next orchestrator restart.",
    fallback: "No interface component is loaded; this is the host-rendered fallback.",
    unknown: "This checkout gives no cache key for the interface aspect, so its freshness cannot be judged.",
  }[ui.serving] || "";

  return el(
    "span",
    { class: "sb-item", title: `${ui.aspect || "ui"} revision ${revision} — ${hint}` },
    el("span", { class: "sb-label" }, "ui"),
    el("span", { class: "sb-mono" }, revision),
    tone && el("span", { class: `sb-flag is-${tone}` }, ui.serving)
  );
}

/** The worker fleet: one process per live conversation. */
function workersItem(sys) {
  const w = sys.workers || {};
  const live = w.live || 0;
  const busy = (w.turning || 0) + (w.building || 0);
  const parts = [`${live} live`];
  if (w.turning) parts.push(`${w.turning} in a turn`);
  if (w.building) parts.push(`${w.building} building`);
  if (w.unknown) parts.push(`${w.unknown} not answering`);

  return el(
    "span",
    {
      class: "sb-item sb-drop-1",
      title: `Worker processes — ${parts.join(", ")}${
        w.rss_kb ? `, ${bytes(w.rss_kb)} resident between them` : ""
      }. One per live conversation; idle ones are reaped and cost nothing to bring back.`,
    },
    el("span", { class: "sb-label" }, "workers"),
    el("span", { class: "sb-mono" }, String(live)),
    busy > 0 && el("span", { class: "sb-flag is-warn" }, `${busy} busy`),
    w.unknown > 0 && el("span", { class: "sb-flag is-err" }, `${w.unknown} quiet`)
  );
}

/* Memory, with a meter. Reported as *used* against total, from MemAvailable —
 * the honest figure for headroom, since MemFree ignores reclaimable cache and
 * reads alarmingly low on a perfectly healthy machine. */
function memoryItem(sys) {
  const h = sys.host || {};
  if (!h.mem_total_kb || h.mem_available_kb == null) return null;
  const used = h.mem_total_kb - h.mem_available_kb;
  const frac = used / h.mem_total_kb;
  const tone = frac > 0.9 ? "err" : frac > 0.75 ? "warn" : null;

  return el(
    "span",
    {
      class: "sb-item sb-drop-2",
      title: `${bytes(used)} of ${bytes(h.mem_total_kb)} in use across the machine, ${bytes(
        h.mem_available_kb
      )} available${h.rss_kb ? `; the gateway itself holds ${bytes(h.rss_kb)}` : ""}.`,
    },
    el("span", { class: "sb-label" }, "mem"),
    el(
      "span",
      { class: "sb-meter" },
      el("span", {
        class: `sb-meter-fill${tone ? ` is-${tone}` : ""}`,
        style: `width:${Math.min(100, Math.round(frac * 100))}%`,
      })
    ),
    el("span", { class: "sb-mono" }, `${bytes(used)} / ${bytes(h.mem_total_kb)}`)
  );
}

function loadItem(sys) {
  const h = sys.host || {};
  if (h.load1 == null) return null;
  const cpus = h.cpus || 1;
  const tone = h.load1 > cpus ? "warn" : null;
  return el(
    "span",
    {
      class: "sb-item sb-drop-3",
      title: `One-minute load average ${h.load1.toFixed(2)} across ${cpus} core${
        cpus === 1 ? "" : "s"
      }. Above ${cpus} means work is queueing for CPU — a release build will do that.`,
    },
    el("span", { class: "sb-label" }, "load"),
    el("span", { class: `sb-mono${tone ? ` is-${tone}` : ""}` }, h.load1.toFixed(2)),
    el("span", { class: "sb-quiet" }, `/ ${cpus}`)
  );
}

// --- formatting --------------------------------------------------------------

/** KiB to a one-decimal human size, since every source here is /proc. */
function bytes(kb) {
  if (kb >= 1024 * 1024) return `${(kb / 1024 / 1024).toFixed(1)}G`;
  if (kb >= 1024) return `${(kb / 1024).toFixed(0)}M`;
  return `${kb}K`;
}

function duration(seconds) {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  return `${duration(s)} ago`;
}
