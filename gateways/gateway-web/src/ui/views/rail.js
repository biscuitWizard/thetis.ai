/* The context rail: one persistent, non-modal home for every inspector.
 *
 * A vertical strip of tabs on the right edge — Branch, Files, Context, Skills,
 * Tools, Models — and a docked panel beside it. The panel reflows the chat
 * rather than covering it, so the transcript stays readable and the composer
 * stays typable with anything open. One tab at a time; Files and Context get a
 * wider panel; Escape or a second click on the active tab collapses back to
 * the strip.
 *
 * This replaces the modal slide-over host: `open` takes the exact config the
 * old panel took, so every inspector moved here without rewriting its content.
 */

import { $, clear, el, icon } from "../lib/dom.js";

const X = ["M5 5l10 10", "M15 5l-10 10"];

let tabs = [];
let strip = null;
let panel = null;
let current = null;
/** Set when the user collapses the rail themselves; stops auto-opens. */
let userClosed = false;

export function mountRail(tabList) {
  tabs = tabList;
  strip = $("rail-tabs");
  panel = $("rail-panel");

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && current) close(true);
  });

  drawStrip();
}

export function isOpen(id) {
  return current === id;
}

export function activeTab() {
  return current;
}

/** True until the user collapses the rail by hand. */
export function wantsAutoOpen() {
  return !userClosed && !current;
}

export function close(byUser = false) {
  if (byUser) userClosed = true;
  current = null;
  panel.hidden = true;
  panel.className = "rail-panel";
  clear(panel);
  drawStrip();
}

/** Activates a tab by id — the tab's own `activate` does the drawing. */
export function show(id) {
  tabs.find((t) => t.id === id)?.activate();
}

/**
 * Opens (or re-renders) the panel for one tab. Same config the old modal
 * panel took, plus optional `head` nodes rendered beside the title.
 */
export function open(config) {
  const tab = tabs.find((t) => t.id === config.id);
  current = config.id;
  panel.hidden = false;
  panel.className = `rail-panel${tab?.wide ? " is-wide" : ""}`;

  const body = config.blocks
    ? el("div", { class: "panel-list" }, config.blocks.filter(Boolean))
    : config.items === undefined
      ? el("div", { class: "panel-note" }, "Loading…")
      : config.items.length === 0
        ? el("div", { class: "panel-note" }, config.empty || "Nothing to show.")
        : el("div", { class: "panel-list" }, config.items.map(config.renderItem));

  clear(panel).append(
    el(
      "header",
      { class: "panel-head" },
      el(
        "div",
        { class: "panel-heading" },
        el("h2", { class: "panel-title" }, config.title),
        config.subtitle && el("p", { class: "panel-sub" }, config.subtitle)
      ),
      el(
        "div",
        { class: "panel-head-actions" },
        ...(config.head || []),
        el(
          "button",
          { class: "icon-btn sm", title: "Collapse (Esc)", "aria-label": "Collapse", onClick: () => close(true) },
          icon(X, { size: 14, width: 1.8 })
        )
      )
    ),
    el("div", { class: "panel-body" }, body)
  );

  drawStrip();
}

function drawStrip() {
  if (!strip) return;
  clear(strip).append(
    ...tabs.map((tab) =>
      el(
        "button",
        {
          type: "button",
          class: `rail-tab${current === tab.id ? " is-active" : ""}`,
          title: tab.hint || tab.label,
          "aria-label": tab.label,
          onClick: () => {
            if (current === tab.id) close(true);
            else {
              userClosed = false;
              tab.activate();
            }
          },
        },
        tab.icon()
      )
    )
  );
}

// --- tab icons ------------------------------------------------------------------

function svg(children, viewBox = "0 0 20 20") {
  const node = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  node.setAttribute("viewBox", viewBox);
  node.setAttribute("width", "17");
  node.setAttribute("height", "17");
  node.setAttribute("aria-hidden", "true");
  for (const [tag, attrs] of children) {
    const child = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [key, value] of Object.entries(attrs)) child.setAttribute(key, value);
    child.setAttribute("fill", attrs.fill ?? "none");
    if ((attrs.fill ?? "none") === "none") {
      child.setAttribute("stroke", "currentColor");
      child.setAttribute("stroke-width", attrs["stroke-width"] ?? "1.6");
      child.setAttribute("stroke-linecap", "round");
      child.setAttribute("stroke-linejoin", "round");
    }
    node.append(child);
  }
  return node;
}

export const ICONS = {
  branch: () =>
    svg([
      ["circle", { cx: "5.5", cy: "5", r: "2.2" }],
      ["circle", { cx: "5.5", cy: "15", r: "2.2" }],
      ["circle", { cx: "14.5", cy: "10", r: "2.2" }],
      ["path", { d: "M5.5 7.2v5.6M7.7 10h4.6" }],
    ]),
  todo: () =>
    svg([
      ["rect", { x: "3", y: "3", width: "3", height: "3", rx: ".5" }],
      ["rect", { x: "3", y: "8.5", width: "3", height: "3", rx: ".5" }],
      ["rect", { x: "3", y: "14", width: "3", height: "3", rx: ".5" }],
      ["path", { d: "M8.5 4.5h8M8.5 10h8M8.5 15.5h8" }],
    ]),
  files: () =>
    svg([["path", { d: "M2.5 5.5A1.5 1.5 0 0 1 4 4h3.6L9.4 6h6.6a1.5 1.5 0 0 1 1.5 1.5v7A1.5 1.5 0 0 1 16 16H4a1.5 1.5 0 0 1-1.5-1.5v-9Z" }]]),
  context: () =>
    svg([
      ["path", { d: "M7.5 4 3.5 10l4 6" }],
      ["path", { d: "M12.5 4l4 6-4 6" }],
    ]),
  skills: () =>
    svg([
      ["path", { d: "M10 3l1.6 4.4L16 9l-4.4 1.6L10 15l-1.6-4.4L4 9l4.4-1.6L10 3Z" }],
      ["path", { d: "M15.6 13.4l.6 1.7 1.7.6-1.7.6-.6 1.7-.6-1.7-1.7-.6 1.7-.6.6-1.7Z", "stroke-width": "1.3" }],
    ]),
  tools: () =>
    svg([
      ["path", { d: "M12 5.3a3.6 3.6 0 0 0-4.9 4.6l-3.7 3.7a1.7 1.7 0 0 0 2.4 2.4l3.7-3.7a3.6 3.6 0 0 0 4.6-4.9l-2.3 2.3-2-2 2.2-2.4Z" }],
    ]),
  models: () =>
    svg([
      ["rect", { x: "5", y: "5", width: "10", height: "10", rx: "2" }],
      ["path", { d: "M8 2v2.5M12 2v2.5M8 15.5V18M12 15.5V18M2 8h2.5M2 12h2.5M15.5 8H18M15.5 12H18", "stroke-width": "1.4" }],
    ]),
};
