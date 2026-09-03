/* The conversation sidebar: navigation and nothing else.
 *
 * Search filters as you type; active conversations group by recency; archived
 * ones live in a collapsed section at the bottom, openable and restorable
 * without a trip to /admin.
 *
 * Every row also says what its conversation is doing *right now*, from the
 * host's activity snapshots (`store.activity`): a working conversation shows
 * its current step under a moving sheen and how long the turn has run, one
 * that stopped to ask a question says so, one that failed says how. This is
 * what makes a conversation you are not looking at visibly alive — before,
 * the only row that ever changed was the one on screen, because only its
 * frames reached this tab.
 */

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";

let query = "";

/** One pass of the sheen over a working row. Matches `--sheen` in app.css. */
const SHEEN_MS = 2600;

/** What each built-in step is called. A tool's own name is used as-is. */
const STEP_LABEL = {
  starting: "Starting up",
  thinking: "Thinking",
  writing: "Writing a reply",
  compacting: "Compacting context",
};

/** How the last turn ended, when that is worth a word on the row. */
const OUTCOME_LABEL = {
  cancelled: "Stopped by you",
  restarted: "Interrupted by a restart",
};

/** The activity snapshot for a conversation, or an idle stand-in. */
export const activityOf = (id) => store.activity?.[id] || { state: "idle" };

/* Durations, short enough for a 272px column: "12s", "4m", "1h 12m". Elapsed
 * time reads differently from time-since — "2m" beside a working row means it
 * has been at it for two minutes; beside an idle row it means two minutes
 * ago — so the row's dot and wording carry that distinction, not the number. */
export function fmtDuration(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d`;
}

export function fmtAgo(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 45) return "now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/* The second line's text for a working row: the step, then the tallies.
 *
 * A built-in step gets words; a tool is named bare, in mono. "Running
 * web-search" said nothing "web-search" under a moving sheen does not, and the
 * verb cost the width that made the tool's own name unreadable at 272px. */
export function describeStep(activity) {
  const step = activity.step || "starting";
  const label = STEP_LABEL[step] || null;
  const facts = [];
  if (activity.steps > 1) facts.push(`${activity.steps} steps`);
  if (activity.agents > 0) facts.push(`${activity.agents} ${activity.agents === 1 ? "agent" : "agents"}`);
  if (activity.cost >= 0.005) facts.push(`$${activity.cost.toFixed(2)}`);
  return { label, tool: label ? null : step, facts };
}

/* A sub-agent's row, nested under the conversation that spawned it.
 *
 * Deliberately not a sibling of the conversations. A child has no composer and
 * cannot be talked into, so a top-level row would be an invitation to do the
 * one thing it does not support; indenting under the owner is also the only
 * honest picture of what a sub-agent *is* — work belonging to a conversation,
 * not a conversation of its own.
 *
 * Clicking reveals the child's block in the transcript instead of navigating.
 * That is where its output already is, in full. */
const agentRow = (agent, onReveal) =>
  el(
    "button",
    {
      class: `session-agent is-${agent.state === "running" ? "running" : agent.state === "done" ? "done" : "bad"}`,
      title:
        agent.state === "running"
          ? `${agent.label} is working — click to show it in the transcript`
          : `${agent.label} ${agent.state}${agent.cost ? ` · $${agent.cost.toFixed(4)}` : ""}`,
      onClick: () => onReveal(agent.id),
    },
    el("span", { class: "session-agent-dot" }),
    el("span", { class: "session-agent-label" }, agent.label),
    el(
      "span",
      { class: "session-agent-state" },
      agent.state === "running" ? "working" : agent.state === "done" ? "done" : agent.state
    )
  );

export function mountSessions({ onOpen, onNew, onUnarchive, onRevealAgent }) {
  const reveal = onRevealAgent || (() => {});
  const list = $("session-list");
  const search = $("session-search");
  $("new-chat").addEventListener("click", onNew);

  search.addEventListener("input", () => {
    query = search.value.trim().toLowerCase();
    draw();
  });
  // "/" focuses search from anywhere that is not already an input.
  document.addEventListener("keydown", (event) => {
    if (event.key !== "/" || event.ctrlKey || event.metaKey || event.altKey) return;
    const tag = document.activeElement?.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA") return;
    event.preventDefault();
    search.focus();
  });

  const matches = (session) =>
    !query ||
    (session.title || "").toLowerCase().includes(query) ||
    (session.preview || "").toLowerCase().includes(query);

  /* The clock at the row's right edge. It carries what it measures from in
   * `data-since` so the ticker can move every clock without redrawing the
   * list, which would drop a hover and restart every sheen. */
  const clock = (activity, session) => {
    const working = activity.state === "working";
    const since = working ? activity.since_ms : session.updated_ms || session.created_ms;
    if (!since) return null;
    return el(
      "span",
      {
        class: `session-when${working ? " is-elapsed" : ""}`,
        dataset: { since: String(since), mode: working ? "elapsed" : "ago" },
        title: working
          ? `Working since ${new Date(since).toLocaleTimeString()}`
          : `Last activity ${new Date(since).toLocaleString()}`,
      },
      working ? fmtDuration(Date.now() - since) : fmtAgo(Date.now() - since)
    );
  };

  /* The second line: the live step while working, why it stopped when that
   * needs a person, else the last message's preview. */
  const statusLine = (activity, session) => {
    switch (activity.state) {
      case "working": {
        const { label, tool, facts } = describeStep(activity);
        return el(
          "div",
          { class: "session-line is-working" },
          el("span", { class: "session-dot" }),
          el(
            "span",
            { class: `session-step${tool ? " session-tool" : ""}`, title: tool ? `Running the ${tool} tool` : label },
            label || tool
          ),
          facts.length ? el("span", { class: "session-facts" }, facts.join(" · ")) : null
        );
      }
      case "waiting":
        return el(
          "div",
          { class: "session-line is-waiting" },
          el("span", { class: "session-dot" }),
          el("span", { class: "session-step" }, "Waiting for your answer")
        );
      case "failed":
        return el(
          "div",
          { class: "session-line is-failed" },
          el("span", { class: "session-dot" }),
          el(
            "span",
            { class: "session-step", title: activity.outcome || "" },
            "Stopped: ",
            el("span", { class: "session-tool" }, activity.outcome || "error")
          )
        );
      default: {
        // An ordinary stop shows the conversation; an unusual one — cancelled,
        // interrupted — is worth the one line it takes to say so.
        const note = OUTCOME_LABEL[activity.outcome];
        return el(
          "div",
          { class: `session-line${note ? " is-noted" : ""}` },
          note || session.preview || "No messages yet"
        );
      }
    }
  };

  const row = (session, { archived } = {}) => {
    const activity = archived ? { state: "idle" } : activityOf(session.id);
    const state = activity.state || "idle";
    const modeTag = session.mode && session.mode !== "agent" ? session.mode : null;
    const hint = [
      session.title || "Untitled",
      state === "working" ? `working — ${(describeStep(activity).label || `running ${activity.step}`).toLowerCase()}` : null,
      state === "waiting" ? "waiting for your answer" : null,
      state === "failed" ? `stopped: ${activity.outcome}` : null,
    ]
      .filter(Boolean)
      .join(" · ");
    return el(
      "div",
      {
        class: `session is-${state}${session.id === store.current ? " is-active" : ""}${archived ? " is-archived" : ""}`,
        dataset: { session: session.id },
        // The sheen's phase, from a shared clock. The list is redrawn on every
        // activity change — each tool call — and a CSS animation restarts with
        // its node, so without this the sheen jumped back to the start on every
        // step. Anchoring the delay to wall time keeps it gliding through a
        // redraw, and keeps every working row in step with the others.
        style: state === "working" ? `--phase: -${Date.now() % SHEEN_MS}ms` : null,
      },
      el(
        "button",
        {
          class: "session-open",
          title: hint,
          onClick: () => onOpen(session.id),
        },
        el(
          "div",
          { class: "session-title" },
          // Somebody else's, when the sidebar is showing everyone's. The host
          // adds `owner` only then, and only to rows that are not the viewer's.
          session.owner && !session.mine
            ? el("span", { class: "session-owner", title: `Belongs to ${session.owner}` }, session.owner_name || session.owner)
            : null,
          el("span", { class: "session-title-text" }, session.title || "Untitled"),
          modeTag ? el("span", { class: "session-mode", title: `Running in ${modeTag} mode` }, modeTag) : null,
          clock(activity, session)
        ),
        statusLine(activity, session)
      ),
      archived
        ? el(
            "button",
            {
              class: "session-restore",
              title: "Bring this conversation back to the list",
              onClick: (event) => {
                event.stopPropagation();
                onUnarchive(session.id);
              },
            },
            "Restore"
          )
        : null
    );
  };

  const heading = (label, { count, working } = {}) =>
    el(
      "div",
      { class: "session-group" },
      el("span", {}, label),
      count != null ? el("span", { class: "session-count" }, String(count)) : null,
      working
        ? el(
            "span",
            { class: "session-count is-working", title: `${working} of these ${working === 1 ? "is" : "are"} mid-turn` },
            `${working} working`
          )
        : null
    );

  const draw = () => {
    clear(list);

    const all = store.sessions || [];
    const active = all.filter((s) => !s.archived && matches(s));
    const archived = all.filter((s) => s.archived && matches(s));

    // Say so when the list is everyone's, because otherwise the only clue is
    // a lit button in the header.
    if (store.viewAll) {
      list.append(
        el("div", { class: "session-everyone" }, "Everyone's conversations — yours are unlabelled")
      );
    }

    if (!all.length) {
      list.append(el("div", { class: "session-empty" }, "No conversations yet — start one with the + button."));
      setTitle(0);
      return;
    }
    if (!active.length && !archived.length) {
      list.append(el("div", { class: "session-empty" }, "Nothing matches — try fewer words, or clear the search."));
      setTitle(countWorking(all));
      return;
    }

    // Recency groups. The list already arrives newest-first; the headings just
    // say where "today" stops.
    const dayStart = new Date();
    dayStart.setHours(0, 0, 0, 0);
    const today = dayStart.getTime();
    const week = today - 6 * 24 * 60 * 60 * 1000;
    const bucket = (s) => {
      const at = s.updated_ms || s.created_ms || 0;
      return at >= today ? "Today" : at >= week ? "This week" : "Earlier";
    };

    const groups = new Map();
    for (const session of active) {
      const group = bucket(session);
      if (!groups.has(group)) groups.set(group, []);
      groups.get(group).push(session);
    }
    for (const [label, sessions] of groups) {
      list.append(heading(label, { working: countWorking(sessions) }));
      for (const session of sessions) {
        list.append(row(session));
        // Only the conversation on screen can have known children: the client
        // learns of a sub-agent from its frames, and frames only arrive for what
        // is being watched. Other rows say "2 agents" from the host's count.
        if (session.id === store.current) {
          for (const agent of store.agents || []) {
            list.append(agentRow(agent, reveal));
          }
        }
      }
    }

    if (archived.length) {
      const details = el(
        "details",
        { class: "session-archived" },
        el("summary", {}, heading("Archived", { count: archived.length })),
        ...archived.map((s) => row(s, { archived: true }))
      );
      // Stay open across redraws once the user opened it.
      details.open = archivedOpen;
      details.addEventListener("toggle", () => (archivedOpen = details.open));
      list.append(details);
    }

    setTitle(countWorking(all.filter((s) => !s.archived)));
    tick();
  };

  let archivedOpen = false;

  const countWorking = (sessions) =>
    sessions.filter((s) => activityOf(s.id).state === "working").length;

  /* The window title carries how many conversations are working, so a tab in
   * the background says "(2) Thetis" and you know without switching to it. */
  const baseTitle = document.title;
  const setTitle = (working) => {
    const wanted = working ? `(${working}) ${baseTitle}` : baseTitle;
    if (document.title !== wanted) document.title = wanted;
  };

  /* Moves every clock forward in place. Once a second while anything is
   * working — the elapsed time on a live row is the thing you watch — and
   * every half minute otherwise, when "4m" becoming "5m" is all that changes.
   * The DOM is written only when the text differs. */
  const tick = () => {
    const now = Date.now();
    for (const node of list.querySelectorAll(".session-when[data-since]")) {
      const since = Number(node.dataset.since);
      const text = node.dataset.mode === "elapsed" ? fmtDuration(now - since) : fmtAgo(now - since);
      if (node.textContent !== text) node.textContent = text;
    }
  };
  let ticker = null;
  const schedule = () => {
    clearInterval(ticker);
    if (document.hidden) return;
    const anyWorking = (store.sessions || []).some((s) => activityOf(s.id).state === "working");
    ticker = setInterval(tick, anyWorking ? 1000 : 30_000);
  };
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) tick();
    schedule();
  });

  store.watch("sessions", draw);
  store.watch("current", draw);
  store.watch("agents", draw);
  store.watch("activity", () => {
    draw();
    schedule();
  });
  draw();
  schedule();
}
