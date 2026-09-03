/* The conversation sidebar: navigation and nothing else.
 *
 * Search filters as you type; active conversations group by recency; archived
 * ones live in a collapsed section at the bottom, openable and restorable
 * without a trip to /admin.
 */

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";

let query = "";

export function mountSessions({ onOpen, onNew, onUnarchive }) {
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
        el("div", { class: "session-title" }, session.title || "Untitled"),
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
  draw();
}
