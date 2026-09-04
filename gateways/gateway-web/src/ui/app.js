/* Thetis web client — entry point.
 *
 * The transcript is a pure function of the session event log: everything the
 * user does is submitted to the host and comes back as an event, so several
 * tabs on one conversation stay in step with no client-side reconciliation.
 *
 * The shell is three zones. The sidebar is navigation only; the middle is a
 * tabbed stage whose first tab is always the conversation, with a tab per
 * sub-agent and per open file after it; every inspector — Branch, Files,
 * Context, Skills, Tools, Models — is a tab in the persistent rail on the
 * right, which docks beside the stage instead of covering it.
 *
 * This file only wires pieces together. Behaviour lives in views/ and lib/.
 */

import { $, clear, el, setHidden } from "./lib/dom.js";
import { Connection } from "./lib/socket.js";
import { store, denied } from "./lib/store.js";
import { toast, popover } from "./lib/toast.js";
import { mountBranch } from "./views/branch.js";
import { mountComposer } from "./views/composer.js";
import { mountSessions } from "./views/sessions.js";
import * as workspace from "./views/workspace.js";
import * as panel from "./views/panel.js";
import * as rail from "./views/rail.js";
import * as todoView from "./views/todo.js";
import * as participantsView from "./views/participants.js";
import * as stage from "./views/stage.js";
import * as context from "./views/context.js";
import * as statusbar from "./views/statusbar.js";
import * as transcript from "./views/transcript.js";
import * as avatars from "./views/avatars.js";
import * as admin from "./views/admin.js";
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

// --- live activity ----------------------------------------------------------

/* Folds activity snapshots into the store, keeping the newer of a held and an
 * incoming one by the host's `rev`. A pushed change and a `sessions` reply
 * travel on different paths and can arrive in either order; merging by arrival
 * left a row saying "working" after the push that said it had finished. */
function mergeActivity(entries) {
  let changed = false;
  const next = { ...store.activity };
  for (const { session, ...snapshot } of entries) {
    const held = next[session];
    if (held && (held.rev ?? 0) > (snapshot.rev ?? 0)) continue;
    next[session] = snapshot;
    changed = true;
  }
  if (changed) store.set({ activity: next });
}

let listTimer = null;
function scheduleList() {
  clearTimeout(listTimer);
  listTimer = setTimeout(() => connection.send({ type: "list" }), 250);
}

// --- connection -------------------------------------------------------------

// A configured avatar is an arbitrary URL, so it can 404, be blocked, or point
// at something that is not an image. The brand is the first thing on screen and
// must not degrade to a broken-image glyph: fall back to the built-in mark,
// which is always in the markup for exactly this reason.
//
// Two bugs were in here, both of the silent kind. The ids were "#"-prefixed
// but `$` is getElementById, so the lookup found nothing and the function
// returned before registering anything. And the mark is an <svg>, which does
// not inherit the `hidden` IDL attribute — `mark.hidden = false` would have set
// a dead JS property, leaving the mark hidden even once it was needed. Hence
// setHidden, which goes through the attribute.
function wireAvatar() {
  const img = $("brand-avatar");
  const mark = $("brand-mark");
  if (!img || !mark) return;
  img.addEventListener("error", () => {
    setHidden(img, true);
    setHidden(mark, false);
  });
}
wireAvatar();

const connection = new Connection({
  onStatus: (state, text) => {
    // A dropped socket cannot be carrying a submission any more, and leaving
    // the composer locked behind a message that will never be acknowledged is
    // the worst of both worlds.
    if (state === "offline") failPending("The connection dropped before the message was accepted.");
    setStatus(state, text);
  },
});

// The turn avatars and the sidebar's avatar button. Mounted before the frame
// handlers, which route `user-avatar` into it, and before the transcript, which
// asks it for a tile per turn.
// `id` is the open conversation on every frame on this socket, and the host
// needs it to fan a change out to the other tabs watching this conversation —
// so it is added here rather than made the view's business.
avatars.mountAvatars((frame) => connection.send({ id: store.current, ...frame }));
participantsView.mountParticipants((frame) => connection.send(frame));

connection.onOpen(() => {
  // Live state held from before the socket dropped may describe a host that
  // has since restarted, in which case every conversation is idle. The
  // `sessions` reply to `hello` carries the current snapshots.
  store.set({ activity: {} });
  connection.send({ type: "hello" });
  // The "everyone's conversations" switch lives on the socket's principal,
  // so a reconnect starts personal again; put it back before the sidebar
  // draws a list that quietly lost half its rows.
  if (store.viewAll) connection.send({ type: "list", all: true });
  if (store.current) connection.send({ type: "open", id: store.current });
  // `hello` already replies with the stored avatar, but it is asked for
  // explicitly too: another tab can have changed it while this socket was down,
  // and a broadcast sent then reached nobody here.
  avatars.request();
});

// Mounted before the frame handlers below, which refer to it. The drawer asks
// for its own list rather than being told: a reconnect, a conversation switch
// and a newly opened shell all funnel through the same request.
const terminals = mountTerminals({
  // The drawer asks for the list whenever a conversation opens. Not for a
  // role that is denied terminals: the host would answer every open with an
  // error toast, for a drawer they cannot use.
  onRequest: (id) => {
    if (!denied("terminal")) connection.send({ type: "terminals", id });
  },
  // `id` is the conversation everywhere on this socket, so the shell goes in
  // its own field rather than overloading it.
  onKill: (terminal) =>
    connection.send({ type: "terminal-close", id: store.current, terminal }),
});

/* The centre stage's tab strip. Mounted before the transcript, which opens a
 * pane for every sub-agent on its first frame, and before the frame handlers,
 * which route `workspace-file` into the editor tabs. */
const centre = stage.mountStage({
  sendFrame: (frame) => connection.send(frame),
  onRename: (title) => {
    connection.send({ type: "rename", id: store.current, title });
    store.set({ title });
    setHeader();
  },
  onRevealInline: (id) => revealInlineAgent(id),
});

/* Shows a sub-agent's work in the conversation itself, rather than in its tab.
 *
 * Goes through the transcript instead of querying the DOM for the block: the
 * inline copy's rows are built lazily, and `revealAgent` materialises them
 * before anything measures where to scroll. Both callers — the sidebar row when
 * the tab has gone, and the "Show in conversation" button on a sub-agent tab —
 * need identical behaviour, so it lives here once. */
function revealInlineAgent(id) {
  centre.show("chat");
  const block = transcript.revealAgent(id);
  if (!block) {
    toast("That sub-agent's output is no longer on screen.", { tone: "error" });
    return;
  }
  block.scrollIntoView({ behavior: "smooth", block: "center" });
  block.classList.add("is-flashed");
  setTimeout(() => block.classList.remove("is-flashed"), 1200);
}

// A reconnect replays `open` for the same conversation, which `setSession`
// short-circuits — so the drawer would keep whatever it had from before the
// socket dropped, having missed every feed frame in between. Re-ask here.
connection.onOpen(() => {
  // Not for someone whose role withholds terminals: the host would answer
  // every open with an error toast, for a drawer they cannot use.
  if (store.current && !denied("terminal")) connection.send({ type: "terminals", id: store.current });
});

// The "everyone's conversations" switch. Host-enforced: the frame flips a
// per-connection flag on the principal, and `list_sessions` reads it. The
// button only appears for a role that grants `see_all_sessions`.
$("see-all")?.addEventListener("click", () => {
  const on = !store.viewAll;
  if (!connection.send({ type: "list", all: on })) {
    return toast("Not connected.", { tone: "error" });
  }
  applyViewAll(on);
});

function applyViewAll(on) {
  store.set({ viewAll: on });
  const button = $("see-all");
  if (!button) return;
  button.setAttribute("aria-pressed", on ? "true" : "false");
  button.title = on ? "Showing everyone's conversations — click for just yours" : "Show everyone's conversations";
}

connection
  /* Who this socket is for. The host sends it before anything else, so every
   * view can ask `store.user` when it draws. Everything identity-shaped on
   * screen follows from here: the footer, the admin link, the logout form,
   * the see-all switch, and which rail tabs exist at all. */
  .on("user", (frame) => {
    store.set({ user: frame });
    const name = $("user-name");
    if (name) {
      name.textContent = frame.local ? "" : frame.name || frame.id || "";
      name.title = frame.local ? "" : `Signed in as ${frame.id}${frame.role ? ` (${frame.role})` : ""}`;
    }
    setHidden($("admin-link"), !frame.admin);
    admin.onUser(frame);
    setHidden($("logout"), Boolean(frame.local));
    setHidden($("see-all"), !frame.see_all);
    // Admins land on the all-conversations view. For other roles with the
    // explicit grant, the remembered per-connection choice still applies.
    applyViewAll(Boolean(frame.see_all && (frame.admin || frame.viewing_all)));
    // A tab for something the role withholds is not offered. The host refuses
    // the frames anyway; this is so the refusal is never the first thing seen.
    rail.setTabHidden("workspace", frame.workspace === "none");
    rail.setTabHidden("branch", false);
    // In `local` mode there are no accounts, so a conversation has exactly one
    // person in it and there is nobody to invite. The tab would list only
    // yourself and offer an empty picker.
    rail.setTabHidden("participants", Boolean(frame.local));
    if (frame.read_only) {
      document.body.classList.add("is-read-only");
    } else {
      document.body.classList.remove("is-read-only");
    }
  })

  .on("catalog", (frame) => {
    store.set({
      models: frame.models || [],
      modelsHidden: frame.models_hidden || [],
      modes: frame.modes || [],
      modelsRestricted: Boolean(frame.restricted),
    });
    // The catalogue is the panel's whole content, so a change made in any tab
    // redraws it here. The picker follows through its own `models` watcher.
    if (rail.isOpen("models")) drawModels();
    setHeader();
  })


  .on("sessions", (frame) => {
    store.set({ sessions: frame.sessions || [] });
    mergeActivity(
      store.sessions.filter((s) => s.activity).map((s) => ({ session: s.id, ...s.activity }))
    );

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
      todos: null,
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
    // And whether this conversation has a plan. Asked on every open rather than
    // only when a tab is showing, because the plan's tab existing *is* how the
    // user finds out there is one.
    connection.send({ type: "plan", id: frame.session });
    connection.send({ type: "todo", id: frame.session });
  })

  .on("settings", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ mode: frame.mode || "agent", model: frame.model || "" });
    setHeader();
  })

  // The plan document. Owned by the stage, which opens or closes its tab from
  // this frame — so a conversation with a plan shows it on reload, and one
  // without never carries the previous conversation's tab over.
  .on("plan", stage.onPlan)
  .on("todo", (frame) => {
    if (frame.session !== store.current) return;
    todoView.onFrame(frame);
  })
  .on("participants", (frame) => {
    if (frame.session !== store.current) return;
    participantsView.onFrame(frame);
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
    // A plan tool that ran means the document moved. Keyed off the tool's name
    // rather than off `turn-finished` so the tab tracks a plan being built up
    // step by step during a long turn, which is exactly when the user is
    // watching it.
    if (frame.kind === "tool-result" && PLAN_TOOLS.includes(frame.name)) {
      connection.send({ type: "plan", id: store.current });
    }
    if (frame.kind === "tool-result" && TODO_TOOLS.includes(frame.name)) {
      connection.send({ type: "todo", id: store.current });
    }
    transcript.applyEvent(frame);
    invalidate(frame.kind);
  })

  .on("system-status", statusbar.onFrame)

  // A conversation — any conversation this account may see, watched or not —
  // changed what it is doing. The sidebar draws from this; the transcript
  // still learns its own conversation's state from the event stream.
  .on("activity", (frame) => {
    const { type: _type, session, ...snapshot } = frame;
    if (!session) return;
    const before = store.activity[session]?.state;
    mergeActivity([{ session, ...snapshot }]);
    // A turn starting or ending is also when the row's title, preview and
    // recency move — the first message names the conversation — and the list
    // is the only carrier of those for a conversation this tab is not
    // watching. Coalesced: a burst of children settling asks once.
    if (before !== snapshot.state) scheduleList();
  })

  // The user's stored picture, on `hello` and after any tab changes it.
  .on("user-avatar", avatars.onFrame)

  // The control panel's frames. All five land in one place, which owns the
  // panel's state; nothing else on the page reads them.
  .on("admin-overview", admin.onFrame)
  .on("admin-waits", admin.onFrame)
  .on("admin-fields", admin.onFrame)
  .on("admin-entries", admin.onFrame)
  .on("admin-result", admin.onFrame)

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
    store.set({
      tools: frame.tools || [],
      toolGroups: frame.groups || [],
      toolGroupsActive: frame.active || [],
      toolGroupReasons: frame.reasons || {},
      toolGroupingOn: frame.grouping === true,
      toolGroupsRouted: frame.routed === true,
    });
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

  /* Workspace frames feed two surfaces: the Files tab, and the composer's
   * @-mention index. Both get every frame rather than one claiming it — the
   * explorer ignores listings for a directory it is not showing, and the
   * mention index is crawling directories the explorer knows nothing about, so
   * routing by path would need a registry neither of them wants to keep. */
  .on("workspace-list", (frame) => {
    workspace.onList(frame);
    composer.mentions.onList(frame);
  })
  // Fuzzy path search for the composer's `@` menu. Answered host-side from a
  // cached walk: the shared workspace is far too large to index in a browser.
  .on("workspace-find", (frame) => composer.mentions.onFind(frame))
  // Read replies feed the rail's preview and any editor tab on the stage. The
  // stage ignores a path it did not ask for, so a single click in Files cannot
  // stamp over an open editor's draft.
  .on("workspace-file", (frame) => {
    workspace.onFile(frame);
    stage.onFile(frame);
  })
  .on("workspace-result", (frame) => {
    workspace.onResult(frame);
    stage.onResult(frame);
    composer.mentions.onResult(frame);
  })

  .on("debug-request", context.onFrame)

  // The shells this conversation has open, in full: sent on opening a
  // conversation and whenever one is created or reaped.
  .on("terminals", terminals.onList)
  // One piece of live shell activity — a command, a burst of output, an exit.
  // Pushed by the worker as it happens rather than polled: watching a build
  // scroll is the entire point of the drawer.
  .on("terminal", terminals.onFeed)
  // The outcome of the drawer's own kill button. The list is not rebuilt from
  // this: the `closed` feed event that follows is what removes the row, so a
  // shell killed from another tab disappears the same way. This only has to
  // report a failure, since success is already visible.
  .on("terminal-close", (frame) => {
    if (frame.session && frame.session !== store.current) return;
    if (!frame.ok) {
      toast(frame.message || `Could not close ${frame.id || "that shell"}.`, { tone: "error" });
    } else if (frame.note) {
      toast(frame.note, { tone: "info" });
    }
  })

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
    if (/^unknown frame type: (branch-|debug-|turn-|unarchive|terminals?|terminal-|user-avatar|plan|todo|admin)/.test(frame.message || "")) {
      if (/: admin$/.test(frame.message || "")) admin.onUnsupported();
      return;
    }
    // A refusal from Execute belongs on the plan tab, beside the button that
    // caused it, rather than in a toast that outlives the view it refers to.
    if (frame.replying_to === "plan-execute" && stage.onPlanError(frame.message || "That was refused.")) {
      return;
    }
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
  todo: ["tool-result", "turn-finished"],
};

/** Tools whose result means the plan document has changed. Kept in step with
 *  the `plan_*` family in `agents/agent-core/src/tools.rs`. */
const PLAN_TOOLS = ["plan_write", "plan_edit", "plan_append"];
const TODO_TOOLS = ["todo_write", "todo_add", "todo_update"];

/** Event kinds after which the composer's @-mention index cannot be trusted. */
const MENTION_STALED_BY = ["tool-result", "modification", "turn-finished"];

const INVALIDATE_COALESCE_MS = 500;
let invalidateTimer = null;

function invalidate(kind) {
  /* The @-mention index is a derived view too (rule 8), but it is not a rail
   * tab and it must not be refreshed on a timer: it is only read when someone
   * types `@`. So it is marked stale here and re-crawled on demand, whichever
   * tab is open — otherwise the menu would offer files a turn ago deleted. */
  if (MENTION_STALED_BY.includes(kind)) composer.mentions.invalidate();

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
      case "todo":
        if (store.current) connection.send({ type: "todo", id: store.current });
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

/* Which tool groups the user has unfolded. Groups open collapsed — a dozen
 * domains of paragraph-length descriptions is not a scannable list — and this
 * set survives the redraws that `INVALIDATED_BY` triggers, so a group the user
 * opened does not snap shut when the agent builds a tool. */
const openToolGroups = new Set();

/* Sends a new active set and waits for the reply.
 *
 * The host answers with the whole `tools` frame rather than an acknowledgement,
 * so the panel redraws from what the store now holds rather than from what this
 * hoped it wrote. That matters here because the agent repairs the pin on read —
 * always-on groups are forced back in — so an optimistic update could show a
 * state that will not survive the next turn.
 */
function setToolGroups(ids) {
  connection.send({ type: "tool-groups-set", id: store.current, groups: ids });
}

function drawTools() {
  const tools = store.tools || [];
  const groups = store.toolGroups || [];
  const active = new Set(store.toolGroupsActive || []);
  const reasons = store.toolGroupReasons || {};
  const grouping = store.toolGroupingOn === true;
  const routed = store.toolGroupsRouted === true;

  // Bucket by the group the agent's own table puts each tool in. Table order is
  // the panel's order: it is already the order the tool block is serialised in,
  // so the panel reads the way the prompt does.
  const byGroup = new Map();
  for (const tool of tools) {
    const id = tool.group || "extra";
    if (!byGroup.has(id)) byGroup.set(id, []);
    byGroup.get(id).push(tool);
  }

  // Groups the table declares but this mode has no tools for — every BigQuery
  // tool in a read-only mode, say — are still listed, because "attached and
  // empty" and "not attached" are different states and a panel that showed only
  // populated groups could not tell them apart.
  const ordered = [
    ...groups.map((g) => g.id),
    ...[...byGroup.keys()].filter((id) => !groups.some((g) => g.id === id)).sort(),
  ];

  const blocks = ordered.map((id) => {
    const group = groups.find((g) => g.id === id) || { id, brief: "", always_on: false };
    const items = byGroup.get(id) || [];
    const attached = active.has(id);
    const locked = group.always_on === true;

    return panel.collapsibleSection(
      {
        title: group.id,
        mono: true,
        count: items.length,
        note: group.brief,
        open: openToolGroups.has(id),
        onToggle: (open) => {
          if (open) openToolGroups.add(id);
          else openToolGroups.delete(id);
        },
        // No switches until the agent has published a table: without one there
        // is no group vocabulary, so a press could only guess at ids.
        aside: grouping && groups.length
          ? panel.toolGroupAside(group, {
              attached,
              reason: reasons[id],
              locked,
              onToggle: (attach) => {
                const next = attach
                  ? [...active, id]
                  : [...active].filter((a) => a !== id);
                setToolGroups(next);
              },
            })
          : undefined,
      },
      items.map(panel.toolItem)
    );
  });

  // What the panel is actually claiming, said once at the top rather than left
  // to be inferred from which cards are dimmed.
  const attachedCount = ordered.filter((id) => active.has(id)).length;
  const loaded = tools.filter((t) => t.attached !== false).length;
  const parts = [`${tools.length} available in ${store.modeLabel()} mode`];
  if (grouping) {
    parts.push(`${loaded} in the prompt`);
    parts.push(`${attachedCount}/${ordered.length} groups attached`);
  } else {
    parts.push(`${ordered.length} group${ordered.length === 1 ? "" : "s"}`);
  }

  const head = [];
  if (grouping && routed) {
    head.push(
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn sm",
          title:
            "Discard this override and let the agent choose the groups again from its own evidence on the next turn.",
          onClick: () => connection.send({ type: "tool-groups-reset", id: store.current }),
        },
        "Reset routing"
      )
    );
  }

  // One line when the role is doing any withholding, because a tool whose
  // capability is denied is simply absent from the list and nothing else on
  // this tab says why.
  const withheld = store.user?.read_only || (store.user?.denied || []).length > 0;
  const policyNote = withheld
    ? el(
        "p",
        { class: "panel-note is-inline" },
        store.user?.read_only
          ? "Your role is read-only: tools that change things are withheld."
          : "Some tools are withheld by your role."
      )
    : null;

  rail.open({
    id: "tools",
    title: "Tools",
    subtitle: tools.length ? parts.join(" · ") : undefined,
    head,
    items: tools,
    blocks: tools.length
      ? [toolScopeNote(grouping, routed), policyNote, ...blocks].filter(Boolean)
      : undefined,
    empty: "No tools are available in this mode.",
    renderItem: panel.toolItem,
  });
}

/* One line saying whether scoping is doing anything, because every state below
 * it looks the same otherwise: with scoping off, every group reads as attached
 * and no switch appears, which is indistinguishable from a conversation that
 * happened to be routed everything. */
function toolScopeNote(grouping, routed) {
  if (!grouping) {
    return el(
      "p",
      { class: "panel-note is-inline" },
      "Scoping is off, so every tool below is in the prompt. Turn on ",
      el("code", { class: "mono" }, "tool_groups.grouping_enabled"),
      " to attach groups by task instead."
    );
  }
  if (!routed) {
    return el(
      "p",
      { class: "panel-note is-inline" },
      "Nothing has been routed yet — until the first message every group is attached. Send a message, or attach groups by hand."
    );
  }
  return null;
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

  const restricted = Boolean(store.modelsRestricted);
  const handlers = {
    // Only offered when a conversation is open: the catalogue is global, but
    // choosing a model is a per-conversation setting.
    onUse: store.current
      ? (model) => {
          connection.send({ type: "set-model", id: store.current, model: model.id });
          store.set({ model: model.id });
        }
      : null,
    onEdit: restricted
      ? null
      : (model) => {
          editing = model.id;
          drawModels();
        },
    onRemove: restricted
      ? null
      : (model) => {
          connection.send({ type: "model-remove", id: store.current, slug: model.id });
        },
    onRestore: restricted
      ? null
      : (model) => {
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

  if (restricted) {
    blocks.push(
      el(
        "p",
        { class: "panel-section-note" },
        "Your role fixes the model catalogue. You may select an offered model, but cannot add or edit entries."
      )
    );
  }

  blocks.push(
    panel.section({
      title: "In the picker",
      count: models.length,
      note: "Offered for every conversation. A model added here is selectable at once — the slug is passed straight to the provider, so no restart is involved.",
    })
  );
  blocks.push(...models.map(row));

  if (restricted) {
    editing = null;
  } else if (editing === "") {
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
  // Cleared together: a stale group set drawn against a fresh tool list would
  // dim the wrong cards for as long as the round trip takes.
  store.set({
    tools: [],
    toolGroups: [],
    toolGroupsActive: [],
    toolGroupReasons: {},
  });
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
  { id: "todo", label: "Todo", hint: "Todo — what this conversation is working through", icon: rail.ICONS.todo, activate: todoView.openTab },
  { id: "workspace", label: "Files", hint: "Files — the shared workspace every agent reads and writes", icon: rail.ICONS.files, wide: true, activate: () => workspaceView.open() },
  { id: "context", label: "Context", hint: "Context — the exact request the model receives", icon: rail.ICONS.context, wide: true, activate: () => context.openTab() },
  { id: "skills", label: "Skills", hint: "Skills — what reached this conversation's prompt, and why", icon: rail.ICONS.skills, activate: openSkills },
  // Wide, like Files and Context: a tool's description is a paragraph of prose,
  // and in the 380px panel a dozen of them wrapped every three or four words.
  { id: "tools", label: "Tools", hint: "Tools — everything the agent can call here", icon: rail.ICONS.tools, wide: true, activate: openTools },
  { id: "models", label: "Models", hint: "Models — the picker's catalogue: list, edit, add by slug", icon: rail.ICONS.models, activate: openModels },
  // Last, and hidden without accounts: with one person there is nothing to
  // say, and a "People" tab listing only yourself is furniture.
  { id: "participants", label: "People", hint: "People — who else is in this conversation, and what they can do here", icon: rail.ICONS.people, activate: openParticipants },
]);

/** The roster is per-conversation, so the tab asks for it as it opens. */
function openParticipants() {
  participantsView.openTab();
  participantsView.request();
}

/** After switching conversations, the open tab shows the new one's content. */
function refreshOpenTab() {
  switch (rail.activeTab()) {
    case "branch":
      showBranchTab();
      break;
    case "todo":
      todoView.openTab();
      if (store.current) connection.send({ type: "todo", id: store.current });
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
    case "participants":
      openParticipants();
      break;
    // Files and Models are global: nothing to refresh.
  }
}

// --- header -----------------------------------------------------------------

/* The conversation's title lives on its tab now, and the chips it used to sit
 * beside live in the chat pane's own bar. */
function setHeader() {
  centre.setChatTitle(store.title);
  $("chat-sub").textContent = store.mode && store.mode !== "agent" ? store.modeLabel() : "";

  const model = $("chip-model");
  model.hidden = !store.current;
  if (store.current) {
    clear(model).append(
      el("span", { class: "picker-dot" }),
      el("span", { class: "picker-label" }, store.modelLabel())
    );
  }

  const todo = $("chip-todo");
  const list = store.todos;
  todo.hidden = !list?.has_todos;
  if (list?.has_todos) {
    todo.textContent = `todo ${list.done}/${list.total}`;
    todo.classList.toggle("is-done", list.total > 0 && list.done === list.total);
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
store.watch("todos", setHeader);
store.watch("turnStats", () => context.onUsageChanged());
store.watch("liveTurn", () => context.onUsageChanged());

$("chip-model").addEventListener("click", openModels);
$("chip-branch").addEventListener("click", showBranchTab);
$("chip-todo").addEventListener("click", todoView.openTab);
$("chip-spend").addEventListener("click", () => context.openTab("usage"));

// Renaming moved onto the conversation's own tab: click the active chat tab a
// second time and the label becomes an input. views/stage.js owns that, and
// calls back through `onRename` above.

$("archive-chat").addEventListener("click", (event) => {
  if (!store.current) return;
  const id = store.current;
  popover(event.currentTarget, {
    message: "Archive this conversation?",
    detail: "It moves to the sidebar's Archived section — nothing is deleted.",
    confirmLabel: "Archive",
    onConfirm: () => {
      connection.send({ type: "archive", id });
      admin.close();
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
  // The panel takes the place of the stage; choosing a conversation is the
  // way back to it.
  admin.close();
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
  /* Brings a sub-agent's own tab forward.
   *
   * No host round trip: the child's stream is already rendered into its tab, so
   * the sidebar row is a jump link. Falls back to the inline block in the
   * conversation if the tab has gone — which happens only for a child whose
   * conversation is no longer open. */
  onRevealAgent: (id) => {
    if (centre.focusAgent(id)) return;
    revealInlineAgent(id);
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
  // The @-mention index is built from `workspace-list` frames, which the host
  // answers directly — so mentions need the raw socket, not the send path.
  sendFrame: (frame) => connection.send(frame),
});

// The status bar polls the host, so it needs a live socket: it asks on every
// connect, which also covers a reconnect after the orchestrator restarts —
// exactly when the trunk revision and the served UI build have changed.
const status = statusbar.mountStatusbar((frame) => connection.send(frame));
admin.mountAdmin((frame) => connection.send(frame));
$("admin-link")?.addEventListener("click", () => admin.toggle());
connection.onOpen(() => status.refresh());

connection.connect();
composer.focus();
