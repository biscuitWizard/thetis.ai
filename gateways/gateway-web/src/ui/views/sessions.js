/* The conversation sidebar: navigation and nothing else.
 *
 * Search filters as you type; active conversations group by recency; archived
 * ones live in a collapsed section at the bottom, openable and restorable
 * without a trip to /admin.
 */

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";

let query = "";

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

  const row = (session, { archived } = {}) =>
    el(
      "div",
      { class: `session${session.id === store.current ? " is-active" : ""}${archived ? " is-archived" : ""}` },
      el(
        "button",
        {
          class: "session-open",
          title: session.title || "Untitled",
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
          el("span", { class: "session-title-text" }, session.title || "Untitled")
        ),
        el("div", { class: "session-preview" }, session.preview || "no messages yet")
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

  const heading = (label, count) =>
    el(
      "div",
      { class: "session-group" },
      el("span", {}, label),
      count != null ? el("span", { class: "session-count" }, String(count)) : null
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
      list.append(el("div", { class: "session-empty" }, "No conversations yet."));
      return;
    }
    if (!active.length && !archived.length) {
      list.append(el("div", { class: "session-empty" }, "Nothing matches."));
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

    let currentGroup = null;
    for (const session of active) {
      const group = bucket(session);
      if (group !== currentGroup) {
        currentGroup = group;
        list.append(heading(group));
      }
      list.append(row(session));
      // Only the conversation on screen can have known children: the client
      // learns of a sub-agent from its frames, and frames only arrive for what
      // is being watched.
      if (session.id === store.current) {
        for (const agent of store.agents || []) {
          list.append(agentRow(agent, reveal));
        }
      }
    }

    if (archived.length) {
      const details = el(
        "details",
        { class: "session-archived" },
        el("summary", {}, heading("Archived", archived.length)),
        ...archived.map((s) => row(s, { archived: true }))
      );
      // Stay open across redraws once the user opened it.
      details.open = archivedOpen;
      details.addEventListener("toggle", () => (archivedOpen = details.open));
      list.append(details);
    }
  };

  let archivedOpen = false;

  store.watch("sessions", draw);
  store.watch("current", draw);
  store.watch("agents", draw);
  draw();
}
