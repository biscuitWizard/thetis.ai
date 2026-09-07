/* Toasts and anchored confirm/prompt popovers.
 *
 * Together these retire every native dialog the UI used to lean on: outcomes
 * land as toasts instead of alert() or transcript incident lines, and anything
 * destructive (or needing a name) gets a small popover anchored to the control
 * that asked — two steps, in place, stating the consequence.
 */

import { el, icon } from "./dom.js";

const X = ["M5 5l10 10", "M15 5l-10 10"];

// --- toasts -------------------------------------------------------------------

let toastHost = null;

function ensureToastHost() {
  if (!toastHost) {
    toastHost = el("div", { class: "toast-host", role: "status", "aria-live": "polite" });
    document.body.append(toastHost);
  }
  return toastHost;
}

/**
 * Shows a toast. Errors stay until dismissed; everything else fades itself.
 *
 * @param {string} message
 * @param {object} [opts]
 * @param {"info"|"good"|"error"} [opts.tone]
 * @param {{label: string, run: () => void}} [opts.action]  e.g. Undo
 */
export function toast(message, { tone = "info", action } = {}) {
  const host = ensureToastHost();
  const node = el(
    "div",
    { class: `toast is-${tone}` },
    el("span", { class: "toast-text" }, message),
    action &&
      el(
        "button",
        {
          type: "button",
          class: "toast-action",
          onClick: () => {
            action.run();
            dismiss();
          },
        },
        action.label
      ),
    el(
      "button",
      { type: "button", class: "toast-x", title: "Dismiss", "aria-label": "Dismiss", onClick: () => dismiss() },
      icon(X, { size: 11, width: 1.9 })
    )
  );

  let timer = null;
  const dismiss = () => {
    clearTimeout(timer);
    node.classList.add("is-leaving");
    setTimeout(() => node.remove(), 160);
  };
  if (tone !== "error") timer = setTimeout(dismiss, action ? 8000 : 5000);

  host.append(node);
  // A wall of stale toasts is worse than losing one: keep the last few.
  while (host.children.length > 4) host.firstChild.remove();
  return dismiss;
}

// --- popovers -------------------------------------------------------------------

let openPopover = null;

function closePopover() {
  if (!openPopover) return;
  openPopover.dispose();
  openPopover = null;
}

/**
 * An anchored confirm (or prompt, when `input` is given).
 *
 * @param {HTMLElement} anchor       the control that asked
 * @param {object} config
 * @param {string} config.message
 * @param {string} [config.detail]
 * @param {string} [config.confirmLabel]
 * @param {boolean} [config.danger]
 * @param {{placeholder?: string, value?: string, mono?: boolean}} [config.input]
 * @param {(value?: string) => void} config.onConfirm  gets the input's value when there is one
 */
export function popover(anchor, config) {
  closePopover();

  const input = config.input
    ? el("input", {
        class: `field${config.input.mono ? " mono" : ""}`,
        type: "text",
        value: config.input.value || "",
        placeholder: config.input.placeholder || "",
        spellcheck: "false",
      })
    : null;

  const confirm = () => {
    const value = input ? input.value.trim() : undefined;
    if (input && !value) {
      input.focus();
      return;
    }
    closePopover();
    config.onConfirm(value);
  };

  const node = el(
    "div",
    { class: "popover", role: "dialog" },
    el("div", { class: "popover-message" }, config.message),
    config.detail && el("div", { class: "popover-detail" }, config.detail),
    input,
    el(
      "div",
      { class: "popover-actions" },
      el(
        "button",
        {
          type: "button",
          class: `ghost-btn ${config.danger ? "is-danger" : "is-primary"}`,
          onClick: confirm,
        },
        config.confirmLabel || "Confirm"
      ),
      el("button", { type: "button", class: "ghost-btn", onClick: closePopover }, "Cancel")
    )
  );

  document.body.append(node);

  // Place under the anchor, right-aligned, clamped to the viewport.
  const at = anchor.getBoundingClientRect();
  const width = node.offsetWidth;
  const left = Math.max(8, Math.min(at.right - width, window.innerWidth - width - 8));
  const below = at.bottom + 6 + node.offsetHeight <= window.innerHeight - 8;
  node.style.left = `${left}px`;
  node.style.top = below ? `${at.bottom + 6}px` : `${Math.max(8, at.top - node.offsetHeight - 6)}px`;

  const onKey = (event) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      closePopover();
    }
    if (event.key === "Enter" && input) {
      event.preventDefault();
      confirm();
    }
  };
  const onClick = (event) => {
    if (!node.contains(event.target)) closePopover();
  };
  document.addEventListener("keydown", onKey, true);
  setTimeout(() => document.addEventListener("click", onClick, true), 0);

  openPopover = {
    dispose() {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("click", onClick, true);
      node.remove();
    },
  };

  (input || node.querySelector("button"))?.focus();
}
