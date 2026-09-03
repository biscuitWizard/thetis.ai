/* A small dropdown used for the mode and model selectors.
 *
 * Generic on purpose: give it options and a change handler and it renders a
 * pill with a menu. New selectors (temperature, persona, …) reuse it as-is.
 */

import { clear, el, icon, onClickOutside } from "../lib/dom.js";

const CARET = "M5 8l5 5 5-5";

export class Picker {
  /**
   * @param {HTMLElement} mount
   * @param {object} config
   * @param {() => Array<{id, label, note?}>} config.options
   * @param {() => string} config.selected   currently selected id
   * @param {(id: string) => void} config.onSelect
   * @param {(selected) => string} config.render  text shown on the pill
   * @param {(selected) => string} [config.dotClass]  extra class for the dot
   */
  constructor(mount, config) {
    this.mount = mount;
    this.config = config;
    this.open = false;
    this.dispose = null;
    this.draw();
  }

  draw() {
    const selectedId = this.config.selected();
    const label = this.config.render(selectedId);

    const button = el(
      "button",
      {
        type: "button",
        class: "picker-btn",
        title: this.config.title || label,
        onClick: (event) => {
          event.stopPropagation();
          this.toggle();
        },
      },
      el("span", { class: `picker-dot ${this.config.dotClass?.(selectedId) || ""}` }),
      el("span", { class: "picker-label" }, label),
      (() => {
        const caret = icon(CARET, { size: 9 });
        caret.classList.add("caret");
        return caret;
      })()
    );

    clear(this.mount).append(button);
    this.mount.className = `picker${this.open ? " is-open" : ""}`;
    if (this.open) this.mount.append(this.menu(selectedId));
  }

  menu(selectedId) {
    const items = this.config.options().map((option) =>
      el(
        "button",
        {
          type: "button",
          class: `picker-item${option.id === selectedId ? " is-selected" : ""}`,
          onClick: (event) => {
            event.stopPropagation();
            this.close();
            if (option.id !== selectedId) this.config.onSelect(option.id);
          },
        },
        el("div", { class: "picker-item-label" }, option.label),
        option.note && el("div", { class: "picker-item-note" }, option.note)
      )
    );

    return el("div", { class: "picker-menu", role: "listbox" }, items);
  }

  toggle() {
    this.open ? this.close() : this.show();
  }

  show() {
    this.open = true;
    this.draw();
    this.dispose = onClickOutside(this.mount, () => this.close());
  }

  close() {
    if (!this.open) return;
    this.open = false;
    this.dispose?.();
    this.dispose = null;
    this.draw();
  }

  /** Re-renders in place, e.g. after the selection changed elsewhere. */
  refresh() {
    if (!this.open) this.draw();
  }
}
