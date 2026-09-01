import { el } from "../lib/dom.js";
import { store } from "../lib/store.js";
import * as rail from "./rail.js";

const LABEL = { pending: "pending", in_progress: "active", completed: "done", cancelled: "cancelled" };

function row(item) {
  const active = item.status === "in_progress" && item.active_form && item.active_form !== item.content;
  return el(
    "div",
    { class: `todo-item is-${item.status}` },
    el("span", { class: "todo-status", title: LABEL[item.status] || item.status }, item.status === "completed" ? "✓" : item.status === "cancelled" ? "–" : item.status === "in_progress" ? "●" : "○"),
    el("div", { class: "todo-copy" },
      el("div", { class: "todo-content" }, item.content),
      active ? el("div", { class: "todo-active" }, item.active_form) : null,
      el("div", { class: "todo-id mono" }, item.id)
    )
  );
}

export function openTab() {
  const data = store.todos || { items: [], done: 0, total: 0 };
  rail.open({
    id: "todo",
    title: "Todo",
    subtitle: data.total ? `${data.done} of ${data.total} done` : "No list yet",
    blocks: data.items?.length
      ? data.items.map(row)
      : [el("div", { class: "panel-note" }, "No todo list yet — ask the agent to break multi-step work into todos.")],
  });
}

export function onFrame(frame) {
  const had = Boolean(store.todos?.has_todos);
  store.set({ todos: frame });
  if (rail.isOpen("todo")) openTab();
  else if (!had && frame.has_todos && rail.wantsAutoOpen()) openTab();
}
