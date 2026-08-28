/* Thetis web client — entry point.
 *
 * The transcript is a pure function of the session event log: everything the
 * user does is submitted to the host and comes back as an event, so several
 * tabs on one conversation stay in step with no client-side reconciliation.
 *
 * The shell is three zones. The sidebar is navigation only; the middle is the
 * conversation; every inspector — Branch, Files, Context, Skills, Tools,
 * Models — is a tab in the persistent rail on the right, which docks beside
 * the chat instead of covering it.
 *
 * This file only wires pieces together. Behaviour lives in views/ and lib/.
 */

import { $, clear, el } from "./lib/dom.js";
import { Connection } from "./lib/socket.js";
import { store } from "./lib/store.js";
import { toast, popover } from "./lib/toast.js";
import { mountBranch } from "./views/branch.js";
import { mountComposer } from "./views/composer.js";
import { mountSessions } from "./views/sessions.js";
import * as workspace from "./views/workspace.js";
import * as panel from "./views/panel.js";
import * as rail from "./views/rail.js";
import * as context from "./views/context.js";
import * as statusbar from "./views/statusbar.js";
import * as transcript from "./views/transcript.js";
import { mountTerminals } from "./views/terminal.js";

// --- status -----------------------------------------------------------------

const statusEl = $("status");

function setStatus(state, text) {
  statusEl.className = `status is-${state}`;
  statusEl.textContent = text;
}

// Busy state is driven by turn events, so it survives reconnects.
store.watch("busy", (busy) => {
  if (busy) setStatus("busy", "working…");
  else if (statusEl.classList.contains("is-busy")) setStatus("online", "connected");
});

// A submission in flight is its own kind of busy, and one the turn events
// cannot report: the log is silent until the host has accepted the message.
store.watch("pending", (pending) => {
  if (pending) setStatus("busy", "sending…");
  else if (!store.busy && statusEl.classList.contains("is-busy")) {
    setStatus("online", "connected");
  }
});

// --- connection -------------------------------------------------------------

const connection = new Connection({
  onStatus: (state, text) => {
    // A dropped socket cannot be carrying a submission any more, and leaving
    // the composer locked behind a message that will never be acknowledged is
    // the worst of both worlds.
    if (state === "offline") failPending("The connection dropped before the message was accepted.");
    setStatus(state, text);
  },
});

connection.onOpen(() => {
  connection.send({ type: "hello" });
  if (store.current) connection.send({ type: "open", id: store.current });
});

// Mounted before the frame handlers below, which refer to it. The drawer asks
// for its own list rather than being told: a reconnect, a conversation switch
// and a newly opened shell all funnel through the same request.
const terminals = mountTerminals({
  onRequest: (id) => connection.send({ type: "terminals", id }),
});

// A reconnect replays `open` for the same conversation, which `setSession`
// short-circuits — so the drawer would keep whatever it had from before the
// socket dropped, having missed every feed frame in between. Re-ask here.
connection.onOpen(() => {
  if (store.current) connection.send({ type: "terminals", id: store.current });
});

connection
  .on("catalog", (frame) => {
    store.set({
      models: frame.models || [],
      modelsHidden: frame.models_hidden || [],
      modes: frame.modes || [],
    });
    // The catalogue is the panel's whole content, so a change made in any tab
    // redraws it here. The picker follows through its own `models` watcher.
    if (rail.isOpen("models")) drawModels();
    setHeader();
  })

  .on("sessions", (frame) => {
    store.set({ sessions: frame.sessions || [] });

    // The host names a conversation from its first message, so the header
    // follows whatever the list now says.
    const active = store.sessions.find((s) => s.id === store.current);
    if (active && active.title !== store.title) {
      store.set({ title: active.title });
      setHeader();
    }

    const open = store.sessions.filter((s) => !s.archived);
    if (!store.current && open.length) openSession(open[0].id);
    if (!open.length && !store.current) {
      store.set({ title: "" });
      setHeader();
      transcript.showEmpty("No conversations", "Start one with the + button.");
    }
  })

  .on("history", (frame) => {
    const events = frame.events || [];
    // The transcript is about to be rebuilt from the log, so any optimistic row
    // is either now redundant (its event is in `events`) or belongs to a
    // conversation no longer on screen. Either way the composer unlocks.
    settlePending();
    store.set({ creating: false });
    store.set({
      current: frame.session,
      title: frame.title || "Untitled",
      mode: frame.mode || "agent",
      model: frame.model || "",
      busy: false,
      branch: null,
      branchLog: [],
      baseRevision: "",
      branchView: "graph",
      hasMessages: events.some((e) => e.kind === "user"),
    });
    setHeader();
    transcript.replay(events);
    store.set({ branchGraph: null });
    // Reconcile the sidebar. Event broadcasts only reach tabs subscribed to
    // that session, so a conversation this tab was not watching can have been
    // renamed by its own first message — or by another tab — while the row
    // here still says "New chat".
    connection.send({ type: "list" });
    // Where this conversation's sandbox stands; and, for a conversation that
    // has not started yet, what trunk offers to start from.
    connection.send({ type: "branch-status", id: frame.session });
    connection.send({ type: "branch-graph", id: frame.session });
    if (!store.hasMessages && !store.trunkLog.length) {
      connection.send({ type: "branch-trunk-log" });
    }
    // The rail persists across conversations, so whatever tab is open follows
    // to the one now on screen.
    refreshOpenTab();
    // The drawer belongs to the conversation, so it drops the previous one's
    // emulators and asks for this one's shells.
    terminals.setSession(frame.session);
  })

  .on("settings", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ mode: frame.mode || "agent", model: frame.model || "" });
    setHeader();
  })

  .on("opened", (frame) => store.set({ current: frame.session }))

  // Acknowledgement of a `send`. The host answers once `submit` has returned —
  // for a first message, after the branch, worktree and worker exist — so this
  // is the definitive "it got there".
  //
  // It does *not* clear the optimistic row. `submit` returns as soon as the
  // session actor has the message, and the actor appends the `user` event just
  // afterwards, so this frame usually arrives first; removing the row here
  // would blank the transcript until the event landed. Acknowledgement unlocks
  // the composer, the real event replaces the row.
  .on("accepted", (frame) => {
    if (frame.session !== store.current) return;
    ackPending();
  })

  .on("event", (frame) => {
    // The sidebar's title and preview are derived server-side, so refresh the
    // list at the two points they can change. Done before the session check so
    // background conversations stay current too.
    if (frame.kind === "user" || frame.kind === "turn-finished") {
      connection.send({ type: "list" });
    }
    // The status bar describes the whole installation, so a build or a turn in
    // any conversation moves it — checked before the session filter below.
    if (statusbar.REFRESHED_BY.includes(frame.kind)) statusbar.refresh();
    if (frame.session !== store.current) return;
    if (frame.kind === "user") {
      store.set({ hasMessages: true });
      // The real thing has arrived; the optimistic row it stood in for goes
      // before the event is rendered, so the message is never shown twice.
      settlePending();
    }
    // The panel's numbers move with builds and branch operations; the pushed
    // status broadcast covers mutations, this covers turn-driven commits.
    if (frame.kind === "branch-op" || frame.kind === "turn-finished" || frame.kind === "modification") {
      connection.send({ type: "branch-status", id: store.current });
    }
    transcript.applyEvent(frame);
    invalidate(frame.kind);
  })

  .on("system-status", statusbar.onFrame)

  .on("skills", (frame) => {
    if (frame.session !== store.current) return;
    store.set({
      skills: frame.all || [],
      skillsRetrieved: frame.retrieved || [],
      skillsUniversal: frame.universal || [],
      skillDiagnostics: frame.diagnostics || [],
    });
    if (rail.isOpen("skills")) drawSkills();
  })

  .on("tools", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ tools: frame.tools || [] });
    if (rail.isOpen("tools")) drawTools();
  })

  .on("branch-status", (frame) => {
    if (frame.session && frame.session !== store.current) return;
    store.set({ branch: frame });
    // The graph and the status move together.
    connection.send({ type: "branch-graph", id: store.current });
    if (rail.isOpen("branch") && store.branchView === "history") {
      connection.send({ type: "branch-log", id: store.current });
    }
  })

  .on("branch-graph", (frame) => {
    if (frame.session && frame.session !== store.current) return;
    store.set({ branchGraph: frame });
    // The graph is the rail's resting view: open it once there is one to
    // show, unless the user has collapsed the rail themselves.
    if (store.current && rail.wantsAutoOpen()) showBranchTab();
  })

  .on("branch-log", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ branchLog: frame.commits || [] });
    if (rail.isOpen("branch") && store.branchView === "history") drawBranchHistory();
  })

  .on("workspace-list", workspace.onList)
  .on("workspace-file", workspace.onFile)
  .on("workspace-result", workspace.onResult)

  .on("debug-request", context.onFrame)

  // The shells this conversation has open, in full: sent on opening a
  // conversation and whenever one is created or reaped.
  .on("terminals", terminals.onList)
  // One piece of live shell activity — a command, a burst of output, an exit.
  // Pushed by the worker as it happens rather than polled: watching a build
  // scroll is the entire point of the drawer.
  .on("terminal", terminals.onFeed)

  .on("turn-cancel", (frame) => {
    if (frame.session && frame.session !== store.current) return;
    if (!frame.ok) toast(frame.message || "The turn could not be stopped.", { tone: "error" });
  })

  .on("branch-trunk-log", (frame) => {
    store.set({ trunkLog: frame.commits || [] });
  })

  .on("branch-result", (frame) => {
    if (frame.session && frame.session !== store.current) return;
    // Refusals and failures are answers to something the user just clicked,
    // so they land as toasts; successes come back through branch-op events
    // and the status broadcast.
    if (!frame.ok) toast(frame.message, { tone: "error" });
  })

  .on("resync", (frame) => {
    // The host dropped broadcast frames because this tab could not keep up.
    // Whatever was on screen is now missing messages — possibly the user's own,
    // which the optimistic row would quietly retire anyway. Rebuild the open
    // conversation from the log rather than leaving a transcript with holes.
    console.warn(`missed ${frame.missed} frames; resyncing`);
    if (store.current && (frame.sessions || []).includes(store.current)) {
      connection.send({ type: "open", id: store.current });
    }
    connection.send({ type: "list" });
  })

  .on("error", (frame) => {
    // An older host does not speak these protocols; the affected controls
    // simply stay quiet rather than shouting about every probe.
    // The status bar says so in place rather than staying blank, since a host
    // older than the frame is a state the bar itself is meant to reveal.
    if (/^unknown frame type: system-/.test(frame.message || "")) {
      statusbar.onUnsupported();
      return;
    }
    if (/^unknown frame type: (branch-|debug-|turn-|unarchive|terminals?)/.test(frame.message || "")) return;
    // An error naming the frame it answers is that frame's verdict. A `send`
    // that reached the host and then failed — a worker that will not start,
    // say — arrives exactly this way, and the composer is locked behind an
    // optimistic message until it hears. An older host sends no `replying_to`,
    // so fall back to assuming an in-flight submission owns the error: being
    // unlocked with the text back is the safer wrong guess.
    const answers = (kind) =>
      frame.replying_to === kind || (!frame.replying_to && kind === "send");
    if (store.pending && answers("send")) {
      return failPending(frame.message || "The message was refused.");
    }
    if (store.creating && answers("new")) {
      store.set({ creating: false });
      transcript.showEmpty("Could not start a conversation", frame.message || "Try again.");
    }
    toast(frame.message, { tone: "error" });
  });

/* A submitted message, from click to acknowledgement.
 *
 * The transcript is a pure function of the event log, which is the right design
 * and has one hole: nothing is logged until the host has accepted the message,
 * and accepting the first message of a conversation means creating a git
 * branch, a worktree and a worker process first. For those seconds the UI used
 * to show an empty composer over "No messages yet" — indistinguishable from a
 * send that did nothing.
 *
 * So a submission is tracked here: an optimistic row appears at once, the
 * composer locks, and the row is replaced by the real event when it lands. The
 * text is kept until then, so a failure can put it back in the box rather than
 * lose it.
 */

// Long enough that a fast send never flashes it, short enough to answer "is it
// stuck?" before the user asks.
const PENDING_EXPLAIN_MS = 1200;
// The host's own ceiling on materializing a worker is far higher, so this is
// not a timeout — just the point where silence deserves saying out loud.
const PENDING_SLOW_MS = 25000;
// How long an acknowledged message waits for its `user` event before the
// placeholder is dropped. The event normally follows within a tick; this only
// catches a broadcast that never arrives at all.
const ECHO_GRACE_MS = 10000;

let pendingTimers = [];

function clearPendingTimers() {
  pendingTimers.forEach(clearTimeout);
  pendingTimers = [];
}

function beginPending(text, attachments) {
  const first = !store.hasMessages;
  store.set({ pending: { text, attachments, first } });
  transcript.showPending({
    text,
    attachments,
    note: "sending…",
  });

  clearPendingTimers();
  pendingTimers.push(
    setTimeout(() => {
      if (!store.pending) return;
      transcript.setPendingNote(
        first
          ? "preparing this conversation's sandbox — a git branch, a worktree and a worker process"
          : "waiting for the conversation's worker"
      );
    }, PENDING_EXPLAIN_MS),
    setTimeout(() => {
      if (!store.pending) return;
      transcript.setPendingNote(
        "still working on it — a first build of the sandbox can take a while"
      );
      toast("Still preparing this conversation. The message is queued, not lost.", {
        tone: "info",
      });
    }, PENDING_SLOW_MS)
  );
}

/* The host has the message: unlock the composer, but leave the optimistic row
 * standing until the `user` event arrives to replace it (see the `accepted`
 * handler). The row stops advertising progress, since there is none left to
 * report — it is simply waiting to be superseded.
 *
 * A safety net rides along: if the event never comes, the row is dropped rather
 * than left implying the message is still in flight. The composer is already
 * unlocked by then, so nothing is blocked either way.
 */
function ackPending() {
  if (!store.pending) return;
  clearPendingTimers();
  transcript.setPendingNote("accepted");
  store.set({ pending: null, awaitingEcho: true });
  pendingTimers.push(
    setTimeout(() => {
      if (store.awaitingEcho) settlePending();
    }, ECHO_GRACE_MS)
  );
}

/** The real event is in the log (or will never be), so the placeholder goes. */
function settlePending() {
  clearPendingTimers();
  transcript.clearPending();
  store.set({ pending: null, awaitingEcho: false });
}

/** It did not get there. Unlock, say so, and hand the text back. */
function failPending(reason) {
  const held = store.pending;
  // Only an unacknowledged message can be handed back. Once the host has taken
  // it, the text belongs to the log, and putting a copy in the composer would
  // invite sending it twice.
  if (!held) return;
  settlePending();
  composer.restore(held.text, held.attachments);
  toast(`${reason} Your message is back in the composer.`, { tone: "error" });
}

/* Keeping the open tab honest while the agent works.
 *
 * The transcript follows the event stream, but a tab showing the filesystem,
 * the tool manifest or the last request was drawn once and has no way of
 * knowing the agent just changed what it says — so it sat there stale while
 * files appeared and tools were built. These are the events that can
 * invalidate each tab.
 *
 * Only the open tab refreshes, and refreshes are coalesced: a working turn
 * emits tool results in bursts, and one redraw per burst is the point. Branch
 * needs no entry — it has its own status broadcast — and Models is pushed by
 * the host whenever the catalogue changes.
 */
const INVALIDATED_BY = {
  workspace: ["tool-result", "modification", "turn-finished"],
  context: ["assistant", "turn-finished"],
  tools: ["modification", "turn-finished"],
  skills: ["modification", "turn-finished"],
};

const INVALIDATE_COALESCE_MS = 500;
let invalidateTimer = null;

function invalidate(kind) {
  if (!INVALIDATED_BY[rail.activeTab()]?.includes(kind)) return;
  clearTimeout(invalidateTimer);
  invalidateTimer = setTimeout(() => {
    // Read the tab again rather than closing over it: half a second is long
    // enough for someone to have switched, and refreshing the tab they left
    // is a wasted round trip.
    switch (rail.activeTab()) {
      case "workspace":
        workspaceView.refresh();
        break;
      case "context":
        context.onConversationAdvanced();
        break;
      case "tools":
        if (store.current) connection.send({ type: "tools", id: store.current });
        break;
      case "skills":
        if (store.current) connection.send({ type: "skills", id: store.current });
        break;
    }
  }, INVALIDATE_COALESCE_MS);
}

// --- inspector tabs -----------------------------------------------------------

function drawSkills() {
  const all = store.skills || [];
  const retrieved = store.skillsRetrieved || [];
  const universalIds = new Set(store.skillsUniversal || []);
  const diagnostics = store.skillDiagnostics || [];

  // Three groups, because a skill reaches the prompt in three different ways
  // and lumping them together hides the whole mechanism. Universals are always
  // there; retrieved ones were chosen for this conversation; the rest exist and
  // were not chosen, which is just as informative.
  //
  // A skill is listed once. A universal stays under "always present" even when
  // it also ranked, since it was in the prompt either way - but it carries the
  // score across, because most of a small corpus is universal and dropping the
  // score would make the panel imply retrieval never ran.
  const ranked = new Map(retrieved.map((s) => [s.id, s]));
  const universal = all.filter((s) => universalIds.has(s.id)).map((s) => {
    const hit = ranked.get(s.id);
    return hit ? { ...s, score: hit.score, how: hit.how } : s;
  });
  const chosen = retrieved.filter((s) => !universalIds.has(s.id));
  const chosenIds = new Set(chosen.map((s) => s.id));
  const rest = all.filter((s) => !universalIds.has(s.id) && !chosenIds.has(s.id));

  // One scale for both groups, so a bar means the same thing in each.
  const top = retrieved.reduce((m, s) => Math.max(m, s.score || 0), 0);

  const parts = [`${all.length} skill${all.length === 1 ? "" : "s"}`];
  if (universal.length) parts.push(`${universal.length} always present`);
  if (chosen.length) parts.push(`${chosen.length} retrieved here`);

  const blocks = [];

  if (all.length) blocks.push(panel.skillLegend());

  if (diagnostics.length) {
    // Errors first: a skill with an error is not loadable, while a warning is
    // a skill that works and could be better.
    const order = { error: 0, warning: 1, info: 2 };
    const sorted = [...diagnostics].sort(
      (a, b) => (order[a.severity] ?? 3) - (order[b.severity] ?? 3)
    );
    const errors = sorted.filter((d) => d.severity === "error").length;
    blocks.push(
      panel.skillSection({
        title: "Problems",
        count: diagnostics.length,
        note: errors
          ? "A skill with an error is skipped entirely, so it can never be retrieved."
          : "These skills load, but retrieval will find them less reliably.",
      })
    );
    blocks.push(...sorted.map(panel.skillDiagnostic));
  }

  if (universal.length) {
    blocks.push(
      panel.skillSection({
        title: "Always present",
        count: universal.length,
        note: "Named in every system prompt by their brief, whatever the conversation is about.",
      })
    );
    blocks.push(
      ...universal.map((s) =>
        panel.skillItem(s, { mode: s.score ? "ranked" : "tree", top })
      )
    );
  }

  if (chosen.length) {
    blocks.push(
      panel.skillSection({
        title: "Retrieved for this conversation",
        count: chosen.length,
        note: "Ranked against the opening message, then pinned. The same set is reused every turn, which is what keeps the prompt cacheable.",
      })
    );
    blocks.push(...chosen.map((s) => panel.skillItem(s, { mode: "ranked", top })));
  } else if (all.length) {
    // Two different situations, and conflating them would be a lie: either
    // retrieval has not run, or it ran and everything it chose is universal
    // and so already listed above with its score.
    blocks.push(
      panel.skillSection({
        title: "Retrieved for this conversation",
        note: retrieved.length
          ? "Everything ranked highest is already always-present, so retrieval added nothing new. The scores above show what it chose."
          : "Nothing yet. Retrieval runs once, on the first message of a conversation.",
      })
    );
  }

  if (rest.length) {
    blocks.push(
      panel.skillSection({
        title: "Available, not in this prompt",
        count: rest.length,
        note: "Not retrieved here, but the agent can still open any of these by id.",
      })
    );
    blocks.push(...rest.map((s) => panel.skillItem(s, { mode: "tree" })));
  }

  rail.open({
    id: "skills",
    title: "Skills",
    subtitle: all.length ? parts.join(" · ") : undefined,
    items: all,
    blocks: all.length ? blocks : undefined,
    empty: "No skills found. Add a folder with a SKILL.md to the skills/ directory.",
    renderItem: (skill) => panel.skillItem(skill),
  });
}

// The domains tools are grouped into, in the order the panel shows them, each
// with a sentence saying what the group covers. A tool whose group is not in
// this list (a connected MCP tool, say) still appears — after these, by name.
const TOOL_GROUP_ORDER = [
  "Files",
  "Shell",
  "Code & tools",
  "Version control",
  "Git",
  "Skills",
  "Memory",
  "Configuration",
  "Web",
  "Notion",
  "Other",
];

const TOOL_GROUP_NOTE = {
  Files: "Reading and writing files — the shared workspace and the sandbox filesystem.",
  Shell: "Running commands and interactive terminal sessions in the sandbox.",
  "Code & tools": "Editing its own source and tools, managing dependencies, and restarting the orchestrator.",
  "Version control": "The conversation's sandbox branch — status, history, updating from trunk and merging back.",
  Git: "Remote repositories on GitHub, as the app's own [bot] identity — reading files, commits, branches, PRs, and cloning a real working tree.",
  Skills: "Finding, reading, writing and linting skills.",
  Memory: "Durable notes this conversation can set and recall later.",
  Configuration: "Reading and changing settings.",
  Web: "Searching the web and fetching page content.",
  Notion: "Reading and writing a Notion workspace — pages, databases and comments.",
  Other: "Tools outside the named domains, including anything a connected server provides.",
};

/* Which tool groups the user has unfolded. Groups open collapsed — a dozen
 * domains of paragraph-length descriptions is not a scannable list — and this
 * set survives the redraws that `INVALIDATED_BY` triggers, so a group the user
 * opened does not snap shut when the agent builds a tool. */
const openToolGroups = new Set();

function drawTools() {
  const tools = store.tools || [];

  // Bucket by the group the host tagged each tool with, then lay the buckets
  // out in a fixed domain order so the surface reads the same every time; any
  // unexpected group falls in after the known ones, alphabetically.
  const byGroup = new Map();
  for (const tool of tools) {
    const group = tool.group || "Other";
    if (!byGroup.has(group)) byGroup.set(group, []);
    byGroup.get(group).push(tool);
  }
  const ordered = [
    ...TOOL_GROUP_ORDER.filter((g) => byGroup.has(g)),
    ...[...byGroup.keys()].filter((g) => !TOOL_GROUP_ORDER.includes(g)).sort(),
  ];

  const blocks = ordered.map((group) => {
    const items = byGroup.get(group);
    return panel.collapsibleSection(
      {
        title: group,
        count: items.length,
        note: TOOL_GROUP_NOTE[group],
        open: openToolGroups.has(group),
        onToggle: (open) => {
          if (open) openToolGroups.add(group);
          else openToolGroups.delete(group);
        },
      },
      items.map(panel.toolItem)
    );
  });

  rail.open({
    id: "tools",
    title: "Tools",
    subtitle: tools.length
      ? `${tools.length} available in ${store.modeLabel()} mode · ${ordered.length} group${ordered.length === 1 ? "" : "s"}`
      : undefined,
    items: tools,
    blocks: tools.length ? blocks : undefined,
    empty: "No tools are available in this mode.",
    renderItem: panel.toolItem,
  });
}

/* The models inspector: the only tab that writes.
 *
 * `editing` is the slug whose row has the form open, or "" for the add form,
 * or null for neither. It lives out here because every save round-trips through
 * the host and comes back as a fresh catalogue, which redraws the panel — a
 * flag inside the draw would be lost each time.
 */
let editing = null;

function drawModels() {
  const models = store.models || [];
  const hidden = store.modelsHidden || [];

  const handlers = {
    // Only offered when a conversation is open: the catalogue is global, but
    // choosing a model is a per-conversation setting.
    onUse: store.current
      ? (model) => {
          connection.send({ type: "set-model", id: store.current, model: model.id });
          store.set({ model: model.id });
        }
      : null,
    onEdit: (model) => {
      editing = model.id;
      drawModels();
    },
    onRemove: (model) => {
      connection.send({ type: "model-remove", id: store.current, slug: model.id });
    },
    onRestore: (model) => {
      connection.send({ type: "model-restore", id: store.current, slug: model.id });
    },
  };

  const save = ({ slug, label, previous }) => {
    editing = null;
    connection.send({ type: "model-save", id: store.current, slug, label, previous });
  };

  // A row being edited is replaced by the form rather than sprouting one below
  // it, so the panel never shows the same model twice.
  const row = (model) =>
    editing === model.id
      ? panel.modelForm({
          model,
          onSave: save,
          onCancel: () => {
            editing = null;
            drawModels();
          },
        })
      : panel.modelItem(model, { ...handlers, selected: model.id === store.model });

  const blocks = [];

  blocks.push(
    panel.section({
      title: "In the picker",
      count: models.length,
      note: "Offered for every conversation. A model added here is selectable at once — the slug is passed straight to the provider, so no restart is involved.",
    })
  );
  blocks.push(...models.map(row));

  if (editing === "") {
    blocks.push(
      panel.modelForm({
        onSave: save,
        onCancel: () => {
          editing = null;
          drawModels();
        },
      })
    );
  } else {
    blocks.push(
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn is-wide",
          onClick: () => {
            editing = "";
            drawModels();
          },
        },
        "Add a model by slug"
      )
    );
  }

  if (hidden.length) {
    blocks.push(
      panel.section({
        title: "Hidden",
        count: hidden.length,
        note: "Configured in thetis.toml but kept out of the picker. Restoring one puts it back exactly as the file has it.",
      })
    );
    blocks.push(...hidden.map(row));
  }

  rail.open({
    id: "models",
    title: "Models",
    subtitle: `${models.length} in the picker${hidden.length ? ` · ${hidden.length} hidden` : ""}`,
    items: models,
    blocks,
    empty: "No models are configured. Add one by slug.",
    renderItem: (model) => panel.modelItem(model),
  });
}

function openModels() {
  editing = null;
  // The catalogue arrives on connect, so there is something to show at once;
  // the request refreshes it in case another tab changed it.
  drawModels();
  connection.send({ type: "models", id: store.current || undefined });
}

function openSkills() {
  if (!store.current) return toast("Open a conversation first — skills are retrieved per conversation.");
  // Show what is already known immediately, then refresh from the host.
  store.set({ skills: [] });
  rail.open({ id: "skills", title: "Skills", items: undefined, renderItem: () => null });
  connection.send({ type: "skills", id: store.current });
}

function openTools() {
  if (!store.current) return toast("Open a conversation first — the tool set depends on its mode.");
  store.set({ tools: [] });
  rail.open({ id: "tools", title: "Tools", items: undefined, renderItem: () => null });
  connection.send({ type: "tools", id: store.current });
}

// The workspace is global — the same shared directory whatever conversation is
// open — so unlike Skills and Tools it works with nothing selected.
const workspaceView = workspace.mountWorkspace((frame) => connection.send(frame));

context.mountContext((frame) => connection.send(frame));

// --- the branch tab -----------------------------------------------------------

function drawBranchTab() {
  if (store.branchView === "history") return drawBranchHistory();

  const branch = store.branch;
  const bits = [];
  if (branch?.materialized) {
    if (branch.ahead > 0) bits.push(`${branch.ahead} ahead of trunk`);
    if (branch.behind > 0) bits.push(`${branch.behind} behind`);
    if (!bits.length && branch.state) bits.push(branch.state);
  }

  const body = branchGraph.render();
  rail.open({
    id: "branch",
    title: "Branch",
    subtitle: bits.join(" · ") || undefined,
    blocks: body
      ? [body]
      : [el("div", { class: "panel-note" }, store.current ? "Waiting for the branch graph…" : "Open a conversation to see its branch.")],
  });
}

function showBranchTab() {
  store.set({ branchView: "graph" });
  drawBranchTab();
}

function drawBranchHistory() {
  const branch = store.branch || {};
  const commits = store.branchLog || [];
  const head = commits[0]?.rev;

  const bits = [];
  if (branch.branch) bits.push(branch.branch);
  if (branch.ahead > 0) bits.push(`${branch.ahead} ahead of trunk`);
  if (branch.behind > 0) bits.push(`${branch.behind} behind`);

  const mine = commits.filter((c) => !c.on_trunk);
  const shared = commits.filter((c) => c.on_trunk);

  const blocks = [
    el(
      "button",
      { type: "button", class: "ghost-btn", onClick: showBranchTab },
      "← Back to the graph"
    ),
  ];
  if (mine.length) {
    blocks.push(
      panel.section({
        title: "This conversation",
        count: mine.length,
        note: "Every green build, skill edit, and checkpoint made here. Reset restores the branch to that point — as a new commit, so nothing is lost.",
      })
    );
    blocks.push(
      ...mine.map((c) =>
        panel.commitItem(c, {
          isHead: c.rev === head,
          onReset: (commit, anchor) => {
            popover(anchor, {
              message: `Reset the branch to ${commit.rev.slice(0, 12)}?`,
              detail: `"${commit.subject}" — history is kept; this adds a new commit restoring that state.`,
              confirmLabel: "Reset branch",
              danger: true,
              onConfirm: () =>
                connection.send({ type: "branch-reset", id: store.current, rev: commit.rev }),
            });
          },
        })
      )
    );
  }
  if (shared.length) {
    blocks.push(
      panel.section({
        title: "From trunk",
        count: shared.length,
        note: "History this branch shares with trunk — its starting point and anything pulled in since.",
      })
    );
    blocks.push(...shared.map((c) => panel.commitItem(c, { isHead: c.rev === head })));
  }

  rail.open({
    id: "branch",
    title: "Branch history",
    subtitle: bits.join(" · ") || undefined,
    items: commits,
    blocks: commits.length ? blocks : [blocks[0], el("div", { class: "panel-note" }, "No commits yet — the first successful build will create one.")],
  });
}

function showBranchHistory() {
  if (!store.current || !store.branch?.materialized) return showBranchTab();
  store.set({ branchView: "history" });
  drawBranchHistory();
  connection.send({ type: "branch-log", id: store.current });
}

const branchGraph = mountBranch({
  onPickBase: (rev, subject, anchor) => {
    if (!store.current || store.hasMessages) return;
    popover(anchor, {
      message: `Start this conversation from ${rev.slice(0, 12)}?`,
      detail: `"${subject}"`,
      confirmLabel: "Start here",
      onConfirm: () => {
        store.set({ baseRevision: rev });
        connection.send({ type: "branch-base", id: store.current, revision: rev });
      },
    });
  },
  onMerge: (anchor) => {
    if (!store.current) return;
    popover(anchor, {
      message: "Merge this conversation's changes to trunk?",
      detail: "Every new conversation will inherit them.",
      confirmLabel: "Merge",
      onConfirm: () => connection.send({ type: "branch-merge", id: store.current }),
    });
  },
  onUpdate: (anchor) => {
    if (!store.current) return;
    popover(anchor, {
      message: "Bring the latest trunk into this conversation?",
      confirmLabel: "Update",
      onConfirm: () => connection.send({ type: "branch-update", id: store.current }),
    });
  },
  onHistory: showBranchHistory,
  onResolve: (anchor) => {
    if (!store.current) return;
    popover(anchor, {
      message: "Hand the conflict to this conversation?",
      detail: "The agent will resolve it and complete the merge.",
      confirmLabel: "Hand it over",
      onConfirm: () => connection.send({ type: "branch-resolve", id: store.current }),
    });
  },
  onAbort: (anchor) => {
    if (!store.current) return;
    popover(anchor, {
      message: "Abort the merge and restore the pre-merge state?",
      confirmLabel: "Abort merge",
      danger: true,
      onConfirm: () => connection.send({ type: "branch-abort", id: store.current }),
    });
  },
  onChange: () => {
    if (rail.isOpen("branch") && store.branchView === "graph") drawBranchTab();
  },
});

// --- the rail ------------------------------------------------------------------

rail.mountRail([
  { id: "branch", label: "Branch", hint: "Branch — this conversation's sandbox: graph, merge, history", icon: rail.ICONS.branch, activate: showBranchTab },
  { id: "workspace", label: "Files", hint: "Files — the shared workspace every agent reads and writes", icon: rail.ICONS.files, wide: true, activate: () => workspaceView.open() },
  { id: "context", label: "Context", hint: "Context — the exact request the model receives", icon: rail.ICONS.context, wide: true, activate: () => context.openTab() },
  { id: "skills", label: "Skills", hint: "Skills — what reached this conversation's prompt, and why", icon: rail.ICONS.skills, activate: openSkills },
  // Wide, like Files and Context: a tool's description is a paragraph of prose,
  // and in the 380px panel a dozen of them wrapped every three or four words.
  { id: "tools", label: "Tools", hint: "Tools — everything the agent can call here", icon: rail.ICONS.tools, wide: true, activate: openTools },
  { id: "models", label: "Models", hint: "Models — the picker's catalogue: list, edit, add by slug", icon: rail.ICONS.models, activate: openModels },
]);

/** After switching conversations, the open tab shows the new one's content. */
function refreshOpenTab() {
  switch (rail.activeTab()) {
    case "branch":
      showBranchTab();
      break;
    case "context":
      context.openTab();
      break;
    case "skills":
      openSkills();
      break;
    case "tools":
      openTools();
      break;
    // Files and Models are global: nothing to refresh.
  }
}

// --- header -----------------------------------------------------------------

function setHeader() {
  $("chat-title").textContent = store.title || "—";
  $("chat-sub").textContent = store.mode && store.mode !== "agent" ? store.modeLabel() : "";

  const model = $("chip-model");
  model.hidden = !store.current;
  if (store.current) {
    clear(model).append(
      el("span", { class: "picker-dot" }),
      el("span", { class: "picker-label" }, store.modelLabel())
    );
  }

  const branch = $("chip-branch");
  const b = store.branch;
  const show = Boolean(store.current && b?.materialized && b.branch);
  branch.hidden = !show;
  if (show) {
    const dot =
      b.state === "conflict" ? " is-err" : b.state === "dirty" ? " is-warn" : " is-ok";
    const bits = [b.branch];
    if (b.ahead > 0) bits.push(`↑${b.ahead}`);
    if (b.behind > 0) bits.push(`↓${b.behind}`);
    clear(branch).append(
      el("span", { class: `picker-dot${dot}` }),
      el("span", { class: "picker-label mono" }, bits.join(" "))
    );
  }

  // The turn in flight counts: a long turn spends real money before it ends,
  // and a meter that only moves afterwards is no use while it runs.
  const spend = $("chip-spend");
  const cost = (store.spendSession || 0) + (store.liveTurn?.cost || 0);
  spend.hidden = !store.current || cost <= 0;
  if (!spend.hidden) {
    spend.textContent = `$${cost.toFixed(4)}`;
    spend.classList.toggle("is-live", Boolean(store.liveTurn));
  }
}

store.watch("mode", setHeader);
store.watch("model", setHeader);
store.watch("branch", setHeader);
store.watch("current", setHeader);
store.watch("spendSession", setHeader);
store.watch("liveTurn", setHeader);
store.watch("turnStats", () => context.onUsageChanged());
store.watch("liveTurn", () => context.onUsageChanged());

$("chip-model").addEventListener("click", openModels);
$("chip-branch").addEventListener("click", showBranchTab);
$("chip-spend").addEventListener("click", () => context.openTab("usage"));

// The title renames in place: click it, type, Enter. Blur cancels — commits
// belong to an explicit keypress, not to focus wandering off.
$("chat-title").addEventListener("click", () => {
  if (!store.current) return;
  const title = $("chat-title");
  const input = el("input", { class: "chat-title-input", type: "text", value: store.title });
  title.replaceWith(input);
  input.focus();
  input.select();

  // Enter restores explicitly and the input's blur fires right after — the
  // second call must be a no-op, not a DOM exception.
  let restored = false;
  const restore = () => {
    if (restored) return;
    restored = true;
    input.replaceWith(title);
  };
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      const next = input.value.trim();
      if (next && next !== store.title) {
        connection.send({ type: "rename", id: store.current, title: next });
        store.set({ title: next });
        title.textContent = next;
      }
      restore();
    }
    if (event.key === "Escape") restore();
  });
  input.addEventListener("blur", restore);
});

$("archive-chat").addEventListener("click", (event) => {
  if (!store.current) return;
  const id = store.current;
  popover(event.currentTarget, {
    message: "Archive this conversation?",
    detail: "It moves to the sidebar's Archived section — nothing is deleted.",
    confirmLabel: "Archive",
    onConfirm: () => {
      connection.send({ type: "archive", id });
      store.set({ current: null, title: "", branch: null, branchGraph: null });
      terminals.setSession(null);
      setHeader();
      transcript.showEmpty("Archived", "Pick another conversation, or start a new one.");
      toast("Conversation archived.", {
        action: { label: "Undo", run: () => connection.send({ type: "unarchive", id }) },
      });
    },
  });
});

// --- actions ----------------------------------------------------------------

function openSession(id) {
  if (id === store.current) return;
  connection.send({ type: "open", id, previous: store.current || undefined });
}

mountSessions({
  onOpen: openSession,
  // Creating a conversation is a host round trip too. It is quick, but "quick"
  // is not "instant", and the + button used to look inert until the new
  // conversation's history came back.
  onNew: () => {
    if (store.creating) return;
    if (!connection.send({ type: "new", previous: store.current || undefined })) {
      return toast("Not connected — cannot start a conversation yet.", { tone: "error" });
    }
    store.set({ creating: true });
    transcript.showWorking("Creating a conversation…", "Setting up its sandbox branch.");
    // A `new` that never answers must not lock the composer for good.
    setTimeout(() => {
      if (!store.creating) return;
      store.set({ creating: false });
      toast("The conversation was not created. Try again.", { tone: "error" });
    }, 20000);
  },
  onUnarchive: (id) => {
    connection.send({ type: "unarchive", id });
    toast("Conversation restored.");
  },
});

transcript.mountTranscript({
  onInspect: () => context.openTab("request"),
  /* Answers from an `ask_user` form go through the ordinary send path: they are
   * a user message, they belong in the log as one, and reusing `send` means the
   * pending row, the composer lock and the failure handling all apply without a
   * second mechanism. Returns whether the socket took it, as the composer does. */
  onAnswer: (text) => {
    if (!store.current) {
      toast("No conversation open — the answers were not sent.", { tone: "error" });
      return false;
    }
    const sent = connection.send({ type: "send", id: store.current, text, attachments: [] });
    if (sent) beginPending(text, []);
    else toast("Not connected — the answers were not sent.", { tone: "error" });
    return sent;
  },
});

const composer = mountComposer({
  onSend: (text, attachments) => {
    const sent = connection.send({ type: "send", id: store.current, text, attachments });
    if (sent) beginPending(text, attachments);
    return sent;
  },
  onSetMode: (mode) => connection.send({ type: "set-mode", id: store.current, mode }),
  onSetModel: (model) => connection.send({ type: "set-model", id: store.current, model }),
  onSetBase: (revision) => {
    store.set({ baseRevision: revision });
    connection.send({ type: "branch-base", id: store.current, revision });
  },
  onShowHistory: showBranchHistory,
  onStop: () => {
    if (store.current) connection.send({ type: "turn-cancel", id: store.current });
  },
});

// The status bar polls the host, so it needs a live socket: it asks on every
// connect, which also covers a reconnect after the orchestrator restarts —
// exactly when the trunk revision and the served UI build have changed.
const status = statusbar.mountStatusbar((frame) => connection.send(frame));
connection.onOpen(() => status.refresh());

connection.connect();
composer.focus();
