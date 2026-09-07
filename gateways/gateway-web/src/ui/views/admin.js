/* The control panel: every operator control and every setting, on the stage.
 *
 * Administrators only. The footer's "control panel" button opens it as a tab
 * with the stage's whole width, because it is tables and forms rather than a
 * list: trunk's history, the worker fleet, the accounts, the model catalogue,
 * and the hundred-odd settings in thetis.toml grouped the way the file groups
 * them. The host-rendered /admin page keeps the same controls in plain HTML
 * for when this UI is the thing that is broken, and is linked from here.
 *
 * Everything here is an affordance, never a check. The host refuses every op
 * for anyone who is not an administrator, and answers `admin-result` with the
 * refusal, which lands beside the control that asked.
 *
 * The panel renders from what the host describes, not from what it knows:
 * settings come with their type, help, source and choices (`admin-fields`),
 * list sections with their columns (`admin-entries`), and manual overrides
 * with their labels and what to ask first (`admin-overview.actions`). A new
 * setting or action in the host shows up here with no change to this file.
 * A new section of the panel is one entry in SECTIONS.
 */

import { clear, el, setHidden } from "../lib/dom.js";
import { toast, popover } from "../lib/toast.js";
import * as stage from "./stage.js";

const TAB = "admin";
const GEAR = [
  "M10 6.5a3.5 3.5 0 1 0 0 7a3.5 3.5 0 0 0 0-7z",
  "M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M4.7 15.3l1.4-1.4M13.9 6.1l1.4-1.4",
];

let send = () => false;
let pane = null;
let nav = null;
let main = null;
let banner = null;
let active = "overview";
let user = null;
let unsupported = false;

/** Everything the host has said, keyed the way the sections read it. */
const state = {
  overview: null,
  actions: [],
  waits: null,
  sections: [],
  fields: null,
  tables: [],
  entries: {},
  /** Outcomes by slot — `set-field:llm.model`, `act:trunk-reset` — so a
   *  message can sit beside the control that caused it across a redraw. */
  results: {},
  /** Which list section has a form open, and for which entry ("" = add). */
  editing: {},
  /** Whether a configuration write succeeded since the last restart. */
  dirty: false,
};

// --- the sections -------------------------------------------------------------

/* Each section says what it needs — `overview`, `waits`, `fields`, or
 * `entries:<list>` — and draws from `state`. Opening a section asks the host
 * for its needs; a reply of that kind redraws whatever section is showing. */
const SECTIONS = [
  { id: "overview", label: "Overview", needs: ["overview", "waits"], render: renderOverview },
  { id: "trunk", label: "Trunk", needs: ["overview"], render: renderTrunk },
  { id: "conversations", label: "Conversations", needs: ["overview"], render: renderConversations },
  { id: "publishing", label: "Publishing", needs: ["overview"], render: renderPublishing },
  { id: "accounts", label: "Accounts", needs: ["overview", "entries:users", "entries:roles", "fields"], render: renderAccounts },
  { id: "models", label: "Models & providers", needs: ["fields", "entries:models", "entries:providers"], render: renderModels },
  { id: "modes", label: "Modes & agent", needs: ["fields", "entries:modes", "entries:subagents.profiles"], render: renderModes },
  { id: "limits", label: "Limits & budgets", needs: ["fields"], render: () => fieldsBlocks(["limits", "budgets", "context", "cache"]) },
  { id: "skills", label: "Skills & tools", needs: ["fields"], render: renderSkillsAndTools },
  { id: "host", label: "Host access", needs: ["fields"], render: () => fieldsBlocks(["filesystem", "terminal", "wasi", "sandbox", "devkit", "browser"]) },
  { id: "server", label: "Server & Discord", needs: ["fields"], render: () => fieldsBlocks(["server", "discord"]) },
  { id: "recovery", label: "Build & recovery", needs: ["overview", "fields"], render: renderRecovery },
];

function section(id) {
  return SECTIONS.find((s) => s.id === id) || SECTIONS[0];
}

// --- talking to the host ------------------------------------------------------

function ask(op, extra = {}) {
  return send({ type: "admin", op, ...extra });
}

function request(needs) {
  for (const need of needs) {
    if (need === "overview") ask("overview");
    else if (need === "waits") ask("waits");
    else if (need === "fields") ask("fields");
    else if (need.startsWith("entries:")) ask("entries", { section: need.slice("entries:".length) });
  }
}

/** Which need a reply satisfies, so only the sections that show it redraw. */
function needOf(frame) {
  switch (frame.type) {
    case "admin-overview": return "overview";
    case "admin-waits": return "waits";
    case "admin-fields": return "fields";
    case "admin-entries": return `entries:${frame.section}`;
    default: return null;
  }
}

// --- lifecycle ------------------------------------------------------------------

/** Called once from app.js. `sender` submits a frame to the host. */
export function mountAdmin(sender) {
  send = sender;
}

export function onUser(frame) {
  user = frame;
}

/** The host answered `admin` with "unknown frame type": an older gateway. */
export function onUnsupported() {
  unsupported = true;
  if (main) draw();
}

/** Opens the panel on a section, building the tab the first time. */
export function open(sectionId = active) {
  if (user && !user.admin) {
    toast("The control panel is for administrators.", { tone: "error" });
    return;
  }
  pane = stage.openTab({
    id: TAB,
    kind: "admin",
    label: "Control panel",
    hint: "Settings, accounts, models, trunk and the worker fleet",
    icon: GEAR,
    build,
    onShow: () => request(section(active).needs),
  });
  show(sectionId);
}

function build(node) {
  nav = el("nav", { class: "admin-nav", "aria-label": "Control panel sections" });
  main = el("div", { class: "admin-main" });
  banner = el("div", { class: "admin-banner is-warn" });
  setHidden(banner, true);

  const head = el(
    "div",
    { class: "stage-bar" },
    el("span", { class: "stage-bar-name" }, "Control panel"),
    el("span", { class: "stage-bar-gap" }),
    el(
      "button",
      {
        type: "button",
        class: "ghost-btn sm",
        title: "Ask the host again for what this section shows",
        onClick: () => request(section(active).needs),
      },
      "Refresh"
    )
  );

  node.append(head, banner, el("div", { class: "admin-shell" }, nav, main));
  drawNav();
}

function show(id) {
  active = section(id).id;
  drawNav();
  draw();
  request(section(active).needs);
}

// --- frames ---------------------------------------------------------------------

/** Every `admin-*` frame lands here. */
export function onFrame(frame) {
  switch (frame.type) {
    case "admin-overview":
      state.overview = frame;
      state.actions = frame.actions || [];
      break;
    case "admin-waits":
      state.waits = frame;
      break;
    case "admin-fields":
      state.sections = frame.sections || state.sections;
      if (frame.prefix) {
        // One section's rows, fresh after a write: swap them in by key, in
        // place, so the rest keep their order and their state.
        const fresh = new Map((frame.fields || []).map((f) => [f.key, f]));
        const kept = (state.fields || []).filter(
          (f) => !(f.section === frame.prefix || f.key.startsWith(`${frame.prefix}.`)) || fresh.has(f.key)
        );
        const merged = kept.map((f) => fresh.get(f.key) || f);
        for (const f of fresh.values()) if (!merged.includes(f)) merged.push(f);
        state.fields = merged;
      } else {
        state.fields = frame.fields || [];
      }
      break;
    case "admin-entries":
      state.tables = frame.tables || state.tables;
      state.entries[frame.section] = frame.entries || [];
      break;
    case "admin-result":
      onResult(frame);
      return;
    default:
      return;
  }
  const need = needOf(frame);
  if (main && section(active).needs.includes(need)) draw();
}

/* An outcome. It is filed under the slot its control reads, and told as a
 * toast as well when the control is not a form field — an action or a
 * restart has no row to sit beside, and a successful one is worth a line. */
function onResult(frame) {
  const slot = slotOf(frame);
  state.results[slot] = { ok: Boolean(frame.ok), message: frame.message || "" };
  if (frame.ok && ["set-field", "save-entry", "remove-entry"].includes(frame.op)) {
    state.dirty = true;
  }
  if (frame.ok && frame.op === "save-entry") state.editing[frame.section] = null;
  if (["act", "sign-out", "restart"].includes(frame.op)) {
    toast(frame.message || (frame.ok ? "Done." : "That was refused."), { tone: frame.ok ? "good" : "error" });
  }
  if (main) draw();
}

function slotOf(frame) {
  switch (frame.op) {
    case "set-field": return `set-field:${frame.key || ""}`;
    case "save-entry":
    case "remove-entry": return `entry:${frame.section || ""}`;
    case "act": return `act:${frame.action || ""}`;
    case "sign-out": return `sign-out:${frame.account || ""}`;
    default: return frame.op || "";
  }
}

// --- drawing --------------------------------------------------------------------

function drawNav() {
  if (!nav) return;
  clear(nav);
  for (const s of SECTIONS) {
    nav.append(
      el(
        "button",
        {
          type: "button",
          class: `admin-nav-btn${s.id === active ? " is-active" : ""}`,
          "aria-current": s.id === active ? "true" : null,
          onClick: () => show(s.id),
        },
        s.label
      )
    );
  }
}

function draw() {
  if (!main) return;
  clear(main);
  drawBanner();

  if (unsupported) {
    main.append(note("This interface was built before the control panel existed; the gateway that serves it does not answer these frames. The host-rendered ", el("a", { href: "/admin", target: "_blank", rel: "noopener" }, "/admin"), " page still works."));
    return;
  }
  if (user && !user.admin) {
    main.append(note("The control panel is for administrators."));
    return;
  }

  const s = section(active);
  main.append(el("h2", { class: "admin-title" }, s.label));
  for (const block of s.render() || []) if (block) main.append(block);
}

function drawBanner() {
  if (!banner) return;
  clear(banner);
  const restartable = state.overview?.restart_available !== false;
  if (!state.dirty) return setHidden(banner, true);
  banner.append(
    ...[
      el("span", {}, "Configuration changed. It is read at startup, so restart Thetis for it to take effect."),
      restartable ? restartButton("Restart now") : el("span", { class: "quiet" }, "Restarting is off (control.allow_restart)."),
      resultNote("restart"),
    ].filter(Boolean)
  );
  setHidden(banner, false);
}

// --- shared pieces --------------------------------------------------------------

function note(...children) {
  return el("p", { class: "panel-note is-inline" }, ...children);
}

function pill(text, cls = "") {
  return el("span", { class: `pill ${cls}`.trim() }, text);
}

/** The filed outcome for a slot, as a line, or nothing. */
function resultNote(slot) {
  const r = state.results[slot];
  if (!r) return null;
  return el("span", { class: `admin-result ${r.ok ? "is-ok" : "is-error"}` }, r.message);
}

function loading() {
  return note("Loading…");
}

function heading(title, help, aside) {
  return el(
    "div",
    { class: "admin-section-head" },
    el("div", {}, el("h3", { class: "panel-section-title" }, title), help ? el("p", { class: "panel-section-note" }, help) : null),
    aside ? el("div", { class: "admin-section-aside" }, ...[].concat(aside).filter(Boolean)) : null
  );
}

function table(columns, rows) {
  return el(
    "div",
    { class: "admin-table-wrap" },
    el(
      "table",
      { class: "admin-table" },
      el("thead", {}, el("tr", {}, ...columns.map((c) => el("th", {}, c)))),
      el("tbody", {}, ...rows.map((cells) => el("tr", {}, ...cells.map((cell) => el("td", {}, cell)))))
    )
  );
}

function short(rev) {
  return (rev || "").slice(0, 12);
}

function money(n) {
  return `$${Number(n || 0).toFixed(4)}`;
}

/** A button for one of the host's manual overrides, asking first when the
 *  host says the action is destructive. */
function actionButton(actionId, target, label) {
  const info = state.actions.find((a) => a.id === actionId);
  if (!info) return null;
  const run = () => {
    delete state.results[`act:${actionId}`];
    ask("act", { action: actionId, target: target || "" });
  };
  return el(
    "button",
    {
      type: "button",
      class: `ghost-btn sm${info.destructive ? " is-danger" : ""}`,
      title: info.description,
      onClick: (event) => {
        if (!info.destructive) return run();
        popover(event.currentTarget, {
          message: info.confirm || `${info.label}?`,
          confirmLabel: info.label,
          danger: true,
          onConfirm: run,
        });
      },
    },
    label || info.label
  );
}

function restartButton(label = "Restart Thetis") {
  return el(
    "button",
    {
      type: "button",
      class: "ghost-btn sm is-danger",
      title: "Restart the orchestrator. Every worker stops; turns in flight end.",
      onClick: (event) =>
        popover(event.currentTarget, {
          message: "Restart the orchestrator?",
          detail: "Every conversation's worker stops and turns in flight end. The new process reads the configuration on the way up.",
          confirmLabel: "Restart",
          danger: true,
          onConfirm: () => {
            delete state.results.restart;
            ask("restart", { reason: "requested from the control panel" });
          },
        }),
    },
    label
  );
}

// --- settings fields --------------------------------------------------------------

function sectionInfo(id) {
  return state.sections.find((s) => s.id === id) || { id, label: id, help: "" };
}

/** The rows of one configuration section, with its heading. */
function fieldsBlock(sectionId) {
  const info = sectionInfo(sectionId);
  const rows = (state.fields || []).filter((f) => f.section === sectionId);
  return el(
    "section",
    { class: "admin-block" },
    heading(info.label, info.help),
    state.fields === null ? loading() : rows.length === 0 ? note("Nothing set here.") : el("div", { class: "admin-fields" }, ...rows.map(fieldRow))
  );
}

function fieldsBlocks(ids) {
  return ids.map(fieldsBlock);
}

const SOURCE_HINT = {
  default: "Not in either file: the built-in default applies.",
  file: "Set in thetis.toml.",
  local: "Set in thetis.local.toml, which is not committed.",
  env: "Overridden by an environment variable for this run; the file's value is not what runs.",
};

/** One setting: its name and help on the left, the control on the right. */
function fieldRow(f) {
  const name = f.key.startsWith(`${f.section}.`) ? f.key.slice(f.section.length + 1) : f.key;
  const locked = !f.editable || f.source === "env";
  const slot = `set-field:${f.key}`;

  const save = (value) => {
    delete state.results[slot];
    ask("set-field", { key: f.key, value: String(value) });
  };

  const badges = [
    pill(f.source, f.source === "env" ? "pill-warn" : f.source === "default" ? "" : "pill-info"),
    f.env ? el("span", { class: "admin-env mono", title: "Environment variable that overrides this setting" }, f.env) : null,
  ];

  return el(
    "div",
    { class: `admin-field${locked ? " is-locked" : ""}`, dataset: { key: f.key } },
    el(
      "div",
      { class: "admin-field-copy" },
      el("div", { class: "admin-field-name" }, el("code", { class: "admin-field-key" }, name), ...badges),
      f.help ? el("p", { class: "admin-field-help" }, f.help) : null,
      el("p", { class: "admin-field-meta" }, SOURCE_HINT[f.source] || "", f.default_value !== "" && f.source !== "default" && !f.secret ? ` Default: ${f.default_value}.` : "")
    ),
    el("div", { class: "admin-field-control" }, control(f, locked, save), resultNote(slot))
  );
}

/** The control for a field, by kind. Saving is one gesture per kind: a toggle
 *  saves itself, a text box saves on Enter or blur, and anything long or
 *  secret has a button, so a half-typed prompt is never written on a click
 *  elsewhere. */
function control(f, locked, save) {
  const kind = f.kind;
  const choices = f.choices || [];

  if (kind === "bool") {
    const box = el("input", { type: "checkbox", class: "admin-toggle", "aria-label": f.key, onChange: () => save(box.checked) });
    box.checked = f.value === "true";
    box.disabled = locked;
    return el("label", { class: "admin-toggle-row" }, box, el("span", {}, f.value === "true" ? "on" : "off"));
  }

  if (choices.length > 0 && ["model", "mode", "role", "provider", "text"].includes(kind)) {
    const select = el("select", { class: "field", "aria-label": f.key, onChange: () => save(select.value) });
    const options = [...choices];
    if (f.value && !options.includes(f.value)) options.unshift(f.value);
    select.append(el("option", { value: "" }, "(unset)"));
    for (const c of options) {
      const opt = el("option", { value: c }, c);
      opt.selected = c === f.value;
      select.append(opt);
    }
    select.disabled = locked;
    return select;
  }

  if (kind === "longtext" || kind === "secret" || kind === "map") {
    const area = kind === "secret"
      ? el("input", { class: "field mono", type: "password", placeholder: f.value === "***" ? "set — type a new value to replace it" : "unset", autocomplete: "new-password", "aria-label": f.key })
      : el("textarea", { class: "field mono admin-textarea", rows: kind === "map" ? "4" : "6", spellcheck: "false", "aria-label": f.key });
    if (kind !== "secret") area.value = f.value;
    area.disabled = locked;
    const button = el(
      "button",
      { type: "button", class: "ghost-btn sm is-primary", onClick: () => { if (kind === "secret" && !area.value) return area.focus(); save(area.value); if (kind === "secret") area.value = ""; } },
      "Save"
    );
    button.disabled = locked;
    return el("div", { class: "admin-control-stack" }, area, el("div", { class: "admin-control-actions" }, kind === "secret" ? pill(f.value === "***" ? "set" : "unset", f.value === "***" ? "pill-on" : "") : null, button));
  }

  // What was last sent, so Enter and the blur that follows a redraw cannot
  // write the same value twice.
  let last = f.value;
  const commit = () => {
    if (input.value === last) return;
    last = input.value;
    save(input.value);
  };
  const numeric = kind === "int" || kind === "float";
  const input = el("input", {
    class: `field${numeric || kind === "path" || kind === "url" || kind === "list" ? " mono" : ""}`,
    type: numeric ? "number" : "text",
    step: kind === "float" ? "any" : kind === "int" ? "1" : null,
    value: f.value,
    placeholder: kind === "list" ? "comma, separated" : f.default_value || "",
    spellcheck: "false",
    "aria-label": f.key,
    onKeydown: (event) => {
      if (event.key === "Enter") { event.preventDefault(); commit(); }
      if (event.key === "Escape") { input.value = last; input.blur(); }
    },
    onBlur: commit,
  });
  input.disabled = locked;
  return input;
}

// --- list sections (models, providers, modes, roles, users, profiles) --------------

function tableInfo(id) {
  return state.tables.find((t) => t.id === id) || null;
}

/** One list section: its entries as cards, an add form, and the edit form
 *  in place of the row being edited. */
function entriesBlock(tableId, opts = {}) {
  const info = tableInfo(tableId);
  const rows = state.entries[tableId];
  const slot = `entry:${tableId}`;
  const editing = state.editing[tableId];

  const addButton = el(
    "button",
    { type: "button", class: "ghost-btn sm", onClick: () => { state.editing[tableId] = editing === "" ? null : ""; draw(); } },
    editing === "" ? "Cancel" : `Add ${opts.singular || "entry"}`
  );

  const blocks = [];
  if (!info || rows === undefined) blocks.push(loading());
  else {
    if (editing === "") blocks.push(entryForm(info, null));
    if (rows.length === 0 && editing !== "") blocks.push(note(opts.empty || "Nothing configured; the built-in default applies."));
    for (const entry of rows) {
      blocks.push(editing === entry.id ? entryForm(info, entry) : entryCard(info, entry, opts));
    }
  }

  return el(
    "section",
    { class: "admin-block" },
    heading(info?.label || tableId, info?.help || "", [addButton]),
    resultNote(slot),
    el("div", { class: "panel-list" }, ...blocks)
  );
}

function entryCard(info, entry, opts) {
  const summary = info.columns
    .filter((c) => c.key !== "id" && entry.fields[c.key] !== undefined && entry.fields[c.key] !== "" && entry.fields[c.key] !== null)
    .map((c) => `${c.key}: ${describeValue(entry.fields[c.key])}`)
    .join(" · ");
  return el(
    "div",
    { class: "card" },
    el(
      "div",
      { class: "card-head" },
      el("div", { class: "card-heading" }, el("h4", { class: "card-title mono" }, entry.id), el("p", { class: "card-meta" }, summary)),
      el("div", { class: "card-badges" }, pill(entry.source, entry.source === "local" ? "pill-info" : ""), ...(opts.badges?.(entry) || []))
    ),
    el(
      "div",
      { class: "card-actions" },
      el("button", { type: "button", class: "ghost-btn sm", onClick: () => { state.editing[info.id] = entry.id; draw(); } }, "Edit"),
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn sm is-danger",
          onClick: (event) =>
            popover(event.currentTarget, {
              message: `Remove ${entry.id} from ${info.label.toLowerCase()}?`,
              detail: "The file is rewritten without it. Takes effect at the next restart.",
              confirmLabel: "Remove",
              danger: true,
              onConfirm: () => { delete state.results[`entry:${info.id}`]; ask("remove-entry", { section: info.id, id: entry.id }); },
            }),
        },
        "Remove"
      )
    )
  );
}

function describeValue(v) {
  if (Array.isArray(v)) return v.join(", ");
  if (v && typeof v === "object") return Object.entries(v).map(([k, x]) => `${k}=${describeValue(x)}`).join(", ");
  return String(v);
}

/** The add-or-edit form: one control per column, typed by the column. */
function entryForm(info, entry) {
  const editing = Boolean(entry);
  const inputs = new Map();

  const rowsEl = info.columns.map((c) => {
    const current = entry?.fields?.[c.key];
    let input;
    if (c.key === "id") {
      input = el("input", { class: "field mono", type: "text", value: entry?.id || "", placeholder: "id", spellcheck: "false" });
      input.disabled = editing;
    } else if (c.kind === "bool") {
      input = el("input", { type: "checkbox", class: "admin-toggle" });
      input.checked = current === true || current === "true";
    } else if (c.kind === "secret") {
      input = el("input", { class: "field mono", type: "password", autocomplete: "new-password", placeholder: current === "***" ? "set — type to replace" : "unset" });
    } else if (c.kind === "longtext") {
      input = el("textarea", { class: "field mono admin-textarea", rows: "4", spellcheck: "false" });
      input.value = current ?? "";
    } else if (c.kind === "map") {
      input = el("textarea", { class: "field mono admin-textarea", rows: "3", spellcheck: "false", placeholder: '{ "key": "value" }' });
      input.value = current && typeof current === "object" ? JSON.stringify(current, null, 1) : "";
    } else if (c.kind === "list" && (c.choices || []).length > 0) {
      // A closed list: a checkbox per option.
      const chosen = new Set(Array.isArray(current) ? current : []);
      input = el("div", { class: "admin-checks" }, ...c.choices.map((opt) => {
        const box = el("input", { type: "checkbox", value: opt });
        box.checked = chosen.has(opt);
        return el("label", { class: "admin-check" }, box, el("span", { class: "mono" }, opt));
      }));
      input.collect = () => [...input.querySelectorAll("input:checked")].map((b) => b.value);
    } else if ((c.choices || []).length > 0) {
      input = el("select", { class: "field" });
      input.append(el("option", { value: "" }, "(unset)"));
      const options = [...c.choices];
      if (current && !options.includes(current)) options.unshift(current);
      for (const opt of options) { const o = el("option", { value: opt }, opt); o.selected = opt === current; input.append(o); }
    } else {
      input = el("input", {
        class: `field${c.kind === "text" ? "" : " mono"}`,
        type: c.kind === "int" || c.kind === "float" ? "number" : "text",
        step: c.kind === "float" ? "any" : null,
        value: Array.isArray(current) ? current.join(", ") : current ?? "",
        placeholder: c.kind === "list" ? "comma, separated" : "",
        spellcheck: "false",
      });
    }
    inputs.set(c.key, input);
    return el(
      "div",
      { class: "admin-form-row" },
      el("label", { class: "admin-form-label" }, el("code", {}, c.key), c.required ? el("span", { class: "admin-required" }, " required") : null, el("span", { class: "admin-form-help" }, c.help)),
      input
    );
  });

  const submit = () => {
    const id = (inputs.get("id")?.value || entry?.id || "").trim();
    if (!id) return inputs.get("id")?.focus();
    const fields = {};
    for (const c of info.columns) {
      if (c.key === "id") continue;
      const input = inputs.get(c.key);
      const had = entry?.fields?.[c.key] !== undefined;
      let value;
      if (c.kind === "bool") value = input.checked;
      else if (input.collect) value = input.collect();
      else value = input.value;
      if (c.kind === "secret") { if (value) fields[c.key] = value; continue; }
      if (c.kind === "map") {
        if (!value.trim()) { if (had) fields[c.key] = null; continue; }
        try { value = JSON.parse(value); } catch (e) { return toast(`${c.key}: ${e.message}`, { tone: "error" }); }
        fields[c.key] = value; continue;
      }
      const empty = value === "" || (Array.isArray(value) && value.length === 0);
      if (empty) { if (had) fields[c.key] = null; continue; }
      fields[c.key] = value;
    }
    delete state.results[`entry:${info.id}`];
    ask("save-entry", { section: info.id, id, fields });
  };

  return el(
    "form",
    { class: "admin-form", onSubmit: (event) => { event.preventDefault(); submit(); } },
    ...rowsEl,
    el(
      "div",
      { class: "model-form-actions" },
      el("button", { type: "button", class: "ghost-btn sm", onClick: () => { state.editing[info.id] = null; draw(); } }, "Cancel"),
      el("button", { type: "submit", class: "ghost-btn sm is-primary" }, editing ? "Save" : "Add")
    )
  );
}

// --- section renderers ----------------------------------------------------------

function renderOverview() {
  const o = state.overview;
  const w = state.waits;
  if (!o) return [loading()];
  const live = o.branches.filter((b) => b.live).length;
  const facts = [
    ["Trunk", el("span", { class: "mono" }, `${o.trunk_name} @ ${short(o.trunk_head)}`)],
    ["Conversations on record", String(o.sessions)],
    ["Workers live", `${live} of ${o.branches.length} branches`],
    ["Accounts", o.local_mode ? "local mode — one implicit administrator" : String(o.accounts.length)],
    ["Configuration", el("span", { class: "mono" }, o.config_path)],
    ["Local overlay", el("span", { class: "mono" }, o.overlay_path)],
  ];
  if (w) {
    facts.push(
      ["Uptime", `${Math.round((w.uptime_s || 0) / 60)} min`],
      ["Turns running", String(w.turns_running ?? 0)],
      ["Building", (w.building || []).length ? (w.building || []).join(", ") : "nothing"],
      ["Build lock", w.build_lock_held ? `held by pid ${w.build_lock_holder_pid || "?"}` : "free"]
    );
  }
  return [
    el("section", { class: "admin-block" }, heading("System", "What the orchestrator is doing right now.", [o.restart_available ? restartButton() : null, resultNote("restart")]), table(["", ""], facts)),
    w
      ? el("section", { class: "admin-block" }, heading("Waits", "What the system is waiting on: workers still materialising, outstanding calls with their age, and who holds the build lock."), el("details", { class: "admin-json" }, el("summary", {}, "Raw waits"), el("pre", { class: "card-pre" }, JSON.stringify(w, null, 2))))
      : null,
  ];
}

function renderTrunk() {
  const o = state.overview;
  if (!o) return [loading()];
  return [
    el(
      "section",
      { class: "admin-block" },
      heading(`Trunk (${o.trunk_name})`, "What every new conversation starts from, and what everyone's page is served from. Trunk only ever advances by merging a conversation's branch; resetting it here is the break-glass path and stops every worker first.", [resultNote("act:trunk-reset")]),
      o.commits.length === 0
        ? note("No commits.")
        : table(
            ["commit", "subject", "author", ""],
            o.commits.map((c) => [
              el("span", { class: "mono" }, short(c.rev), c.head ? " " : "", c.head ? pill("head", "pill-on") : null),
              c.subject,
              el("span", { class: "quiet" }, c.author),
              c.head ? "" : actionButton("trunk-reset", c.rev),
            ])
          )
    ),
  ];
}

function renderConversations() {
  const o = state.overview;
  if (!o) return [loading()];
  return [
    el(
      "section",
      { class: "admin-block" },
      heading("Conversations", "Each conversation runs on its own branch in its own worker process. ↑ commits it has that trunk lacks; ↓ commits trunk has that it lacks. Stopping a worker loses nothing — branch state is on disk and in the log.", [resultNote("act:stop-worker"), resultNote("act:abort-merge"), resultNote("act:release-worktree")]),
      o.branches.length === 0
        ? note("No conversation branches yet.")
        : table(
            ["conversation", "branch", "worker", "↑/↓", "state", "kernel", ""],
            o.branches.map((b) => [
              b.title,
              el("span", { class: "mono" }, b.branch_ref),
              b.live ? pill("live", "pill-on") : el("span", { class: "quiet" }, "stopped"),
              el("span", { class: "mono" }, `↑${b.ahead} ↓${b.behind}`),
              b.state,
              el("span", { class: "mono" }, b.kernel),
              el("span", { class: "admin-actions" }, ...(b.live ? [actionButton("stop-worker", b.session_id), actionButton("abort-merge", b.session_id)] : [actionButton("release-worktree", b.session_id)])),
            ])
          )
    ),
  ];
}

function renderPublishing() {
  const o = state.overview;
  if (!o) return [loading()];
  const dirs = o.private_dirs.length ? o.private_dirs.map((d) => el("code", { class: "mono" }, d)) : [el("em", {}, "nothing")];
  const describe = (id) => state.actions.find((a) => a.id === id)?.description || "";
  const row = (id) => el("div", { class: "admin-action-row" }, actionButton(id), el("span", { class: "admin-action-desc" }, describe(id)), resultNote(`act:${id}`));
  return [
    el(
      "section",
      { class: "admin-block" },
      heading("Publishing", "Directories holding a .thetis-private marker never leave this machine: a filtered public branch mirrors trunk without them, and a pre-push hook refuses everything else."),
      note("Currently private: ", ...dirs.flatMap((d, i) => (i ? [", ", d] : [d])), "."),
      row("export-public"),
      row("push-public"),
      note("When another checkout publishes too, pull before publishing: it merges what they published into trunk here, so the next publish carries both instead of being refused for replacing their work."),
      row("pull-public"),
      row("adopt-remote")
    ),
  ];
}

function renderAccounts() {
  const o = state.overview;
  const blocks = [];
  if (!o) blocks.push(loading());
  else if (o.local_mode) {
    blocks.push(el("section", { class: "admin-block" }, heading("Accounts", "Local mode: one implicit administrator on loopback, no accounts. Switch auth.mode to \"users\" after adding a role with admin = true and a user in it."), ...(!o.admin_enabled ? [note("The admin console is disabled.")] : [])));
  } else {
    blocks.push(
      el(
        "section",
        { class: "admin-block" },
        heading("Accounts", "What the database says about each configured account. \"Sign out everywhere\" ends every login the account holds, on every device. Spend is cumulative across the account's conversations."),
        table(
          ["user", "name", "role", "policy", "conversations", "logins", "spend", ""],
          o.accounts.map((a) => {
            const flags = [a.admin && "admin", a.read_only && "read-only", a.sees_all && "sees all"].filter(Boolean).join(", ");
            const button = el(
              "button",
              {
                type: "button",
                class: "ghost-btn sm",
                onClick: (event) =>
                  popover(event.currentTarget, {
                    message: `Sign ${a.id} out everywhere?`,
                    detail: `Ends ${a.logins} login(s) on every device.`,
                    confirmLabel: "Sign out",
                    danger: true,
                    onConfirm: () => ask("sign-out", { account: a.id }),
                  }),
              },
              "sign out everywhere"
            );
            button.disabled = a.logins === 0;
            return [el("span", { class: "mono" }, a.id), a.name, a.role, el("span", { class: "quiet" }, flags), String(a.conversations), String(a.logins), money(a.spend_usd), el("span", {}, button, resultNote(`sign-out:${a.id}`))];
          })
        )
      )
    );
  }
  blocks.push(entriesBlock("users", { singular: "user", empty: "No accounts configured." }));
  blocks.push(entriesBlock("roles", { singular: "role", empty: "No roles configured." }));
  blocks.push(fieldsBlock("auth"));
  return blocks;
}

function renderModels() {
  return [
    fieldsBlock("llm"),
    entriesBlock("models", { singular: "model", empty: "None configured: the built-in catalogue applies." }),
    note("A model added from the Models tab in the rail lives in the database, per user, and needs no restart; the list above is the installation's catalogue in thetis.toml."),
    entriesBlock("providers", { singular: "provider", empty: "No extra providers; the endpoint under Language model serves everything." }),
  ];
}

function renderModes() {
  return [
    entriesBlock("modes", { singular: "mode", empty: "None configured: the built-in agent and plan modes apply." }),
    fieldsBlock("agent"),
    fieldsBlock("subagents"),
    entriesBlock("subagents.profiles", { singular: "profile", empty: "No profiles." }),
  ];
}

function renderSkillsAndTools() {
  const key = el("input", { class: "field mono", type: "text", placeholder: "tools.notion.version", spellcheck: "false", "aria-label": "setting key" });
  const value = el("input", { class: "field mono", type: "text", placeholder: "value", spellcheck: "false", "aria-label": "setting value" });
  const add = el(
    "form",
    {
      class: "admin-add-key",
      onSubmit: (event) => {
        event.preventDefault();
        const k = key.value.trim();
        if (!k.startsWith("tools.") || k.split(".").length < 3) return toast("A tool setting is tools.<tool>.<key>.", { tone: "error" });
        ask("set-field", { key: k, value: value.value });
        key.value = "";
        value.value = "";
      },
    },
    key,
    value,
    el("button", { type: "submit", class: "ghost-btn sm is-primary" }, "Set")
  );
  return [
    fieldsBlock("skills"),
    fieldsBlock("tool_groups"),
    fieldsBlock("tools"),
    el("section", { class: "admin-block" }, heading("Add a tool setting", "Any key under tools.<tool> is handed to that tool as its configuration. A key ending in token or api_key is kept in the local overlay and never shown again."), add),
  ];
}

function renderRecovery() {
  const o = state.overview;
  return [
    el(
      "section",
      { class: "admin-block" },
      heading("Recovery", "The host-rendered console is served by the orchestrator with no WebAssembly in its path, so it keeps working when every guest is broken.", [o?.restart_available ? restartButton() : null, resultNote("restart")]),
      note(el("a", { href: "/admin", target: "_blank", rel: "noopener" }, "Open /admin"), " — the same controls as Trunk, Conversations, Accounts and Publishing here, as plain forms.", o && !o.admin_enabled ? " The console is currently disabled." : "")
    ),
    ...fieldsBlocks(["control", "build", "watchdog", "paths"]),
  ];
}
