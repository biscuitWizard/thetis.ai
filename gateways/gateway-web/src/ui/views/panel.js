/* Building blocks for the rail's inspector tabs.
 *
 * Opening, closing and the panel chrome live in rail.js now — this module
 * keeps only the renderers: how one skill, tool, model or commit draws. Each
 * inspector supplies these to `rail.open`, so a new inspector is still a
 * render function rather than another dialog.
 */

import { el } from "../lib/dom.js";

// --- skills -----------------------------------------------------------------

/* How a skill got where it is, in words rather than jargon.
 *
 * These strings come from the host's ranker (`How` in skill_index.rs) and mean
 * nothing to a reader on their own, so each is given a plain reading and a
 * longer explanation on hover.
 */
const HOW = {
  dense: {
    label: "semantic match",
    hint: "The opening message and this skill's card were embedded, and their meanings landed close together.",
  },
  lexical: {
    label: "word overlap",
    hint: "Fallback ranking. The embedding call was unavailable, so skills were scored on shared words instead.",
  },
  "parent-of-match": {
    label: "parent of a match",
    hint: "A skill nested inside this one matched, so the parent came along to explain it.",
  },
  "whole-corpus": {
    label: "everything included",
    hint: "The corpus is no larger than the retrieval limit, so ranking was skipped and every skill was included.",
  },
  pinned: {
    label: "pinned earlier",
    hint: "Retrieved for this conversation before scores were recorded, so no score is shown.",
  },
};

/** The four disclosure levels, as a legend. Without this the panel is a list
 *  of cards with no hint that a body and its files sit behind them. */
export function skillLegend() {
  const step = (level, name, note) =>
    el(
      "li",
      { class: "legend-step" },
      el("span", { class: "legend-tag" }, level),
      el("span", {}, el("strong", {}, name), " ", note)
    );

  return el(
    "details",
    { class: "legend" },
    el("summary", {}, "How skills reach the prompt"),
    el(
      "ol",
      { class: "legend-steps" },
      step("L0", "Brief", "— one line. Universal skills are named in every prompt by this alone."),
      step("L1", "Card", "— brief, when to use, nested skills. Added for the skills retrieved below."),
      step("L2", "Body", "— the instructions. Read only when the agent opens the skill by id."),
      step("L3", "Files", "— references, scripts and assets, read one at a time on demand.")
    )
  );
}

/** A section heading, with a sentence saying what the section means. Every
 *  panel that groups its rows needs one: the grouping is the explanation, so it
 *  has to be labelled rather than left as an unexplained gap. */
export function section(spec) {
  return el(
    "div",
    { class: "panel-section" },
    el(
      "div",
      { class: "panel-section-head" },
      el("h3", { class: "panel-section-title" }, spec.title),
      typeof spec.count === "number"
        ? el("span", { class: "panel-section-count" }, String(spec.count))
        : null
    ),
    spec.note && el("p", { class: "panel-section-note" }, spec.note)
  );
}

/** The name the skills panel has always used for it. */
export const skillSection = section;

/** A section whose rows fold away behind its heading.
 *
 * The same heading, count and note as `section`, but as a disclosure holding
 * the rows themselves. Panels with many long rows per group — Tools, where a
 * dozen groups of paragraph-length descriptions made the list unscannable —
 * open collapsed, so the reader picks a domain before reading any prose.
 *
 * @param {object} spec              title, count, note, and `open`
 * @param {Node[]} rows              the group's cards
 * @param {(open:boolean)=>void} [spec.onToggle]  told when the user folds it,
 *   so the caller can remember the state across a redraw
 * @param {Node[]} [spec.aside]      status and controls for the group itself,
 *   laid out on the heading row. A control here must stop its own click from
 *   reaching the summary, or pressing it would fold the group as a side effect.
 * @param {string} [spec.mono]       render the title in the mono face, for a
 *   title that is a literal identifier rather than a display name
 */
export function collapsibleSection(spec, rows) {
  const head = el(
    "summary",
    { class: "panel-group-summary" },
    el(
      "div",
      { class: "panel-section-head" },
      el(
        "h3",
        { class: `panel-section-title${spec.mono ? " mono is-literal" : ""}` },
        spec.title
      ),
      typeof spec.count === "number"
        ? el("span", { class: "panel-section-count" }, String(spec.count))
        : null,
      spec.aside && spec.aside.length
        ? el("div", { class: "panel-section-aside" }, spec.aside.filter(Boolean))
        : null
    ),
    spec.note && el("p", { class: "panel-section-note" }, spec.note)
  );

  const node = el(
    "details",
    { class: "panel-group", open: spec.open === true },
    head,
    el("div", { class: "panel-group-body" }, rows.filter(Boolean))
  );

  if (spec.onToggle) node.addEventListener("toggle", () => spec.onToggle(node.open));
  return node;
}

/** A lint diagnostic. These were counted in the subtitle but never shown, so a
 *  broken skill reported itself as a number and nothing more. */
export function skillDiagnostic(diag) {
  const severity = diag.severity === "error" ? "error" : diag.severity === "warning" ? "warn" : "info";
  return el(
    "article",
    { class: `card diag is-${severity}` },
    el(
      "div",
      { class: "card-head" },
      el(
        "div",
        { class: "card-heading" },
        el("h3", { class: "card-title mono" }, diag.id || "(corpus)"),
        el("p", { class: "card-desc" }, diag.message)
      ),
      el("span", { class: `pill pill-${severity}` }, diag.severity)
    )
  );
}

/** A skill, as an inspector row.
 *
 * There is no switch. What a conversation can see is decided by ranking its
 * opening message, so there is nothing here for a user to turn on; this shows
 * what the corpus holds and how each entry got into the prompt.
 *
 * @param {object} skill
 * @param {object} [opts]
 * @param {"ranked"|"tree"} [opts.mode]  ranked rows show a score, tree rows indent
 * @param {number} [opts.top]            best score in the group, for the bar
 */
export function skillItem(skill, opts = {}) {
  const { mode = "tree", top = 0 } = opts;
  const ranked = mode === "ranked";

  // Nesting is indented rather than stated, so a subtree reads as a subtree.
  // A ranked list is not a tree, though: it is ordered by score, and indenting
  // it would imply a hierarchy the order does not have.
  const indent = !ranked && skill.depth
    ? { style: `margin-left:${skill.depth * 1.25}rem` }
    : {};

  const badges = [];
  if (skill.universal) {
    badges.push(
      el("span", { class: "pill pill-on", title: "Named in every system prompt" }, "universal")
    );
  }
  if (skill.children && skill.children.length) {
    badges.push(
      el(
        "span",
        { class: "pill", title: `Contains ${skill.children.join(", ")}` },
        `${skill.children.length} nested`
      )
    );
  }
  if (skill.resources && skill.resources.length) {
    badges.push(
      el(
        "span",
        { class: "pill", title: `Fetched on demand: ${skill.resources.join(", ")}` },
        `${skill.resources.length} file${skill.resources.length === 1 ? "" : "s"}`
      )
    );
  }

  const how = HOW[skill.how];
  const hasScore = ranked && typeof skill.score === "number" && skill.score > 0;

  // A bare cosine figure means little, so it is drawn relative to the best
  // score in the same group. The number stays for anyone who wants it.
  const score = hasScore
    ? el(
        "div",
        { class: "score", title: how ? how.hint : undefined },
        el(
          "div",
          { class: "score-bar" },
          el("div", {
            class: "score-fill",
            style: `width:${Math.max(4, Math.round((skill.score / (top || skill.score)) * 100))}%`,
          })
        ),
        el("span", { class: "score-num" }, skill.score.toFixed(2)),
        how && el("span", { class: "score-how" }, how.label)
      )
    : ranked && how
      ? el("div", { class: "score", title: how.hint }, el("span", { class: "score-how" }, how.label))
      : null;

  const details = [];
  if (skill.when_to_use) {
    details.push(el("p", { class: "card-desc" }, el("strong", {}, "Use when: "), skill.when_to_use));
  }
  if (skill.children && skill.children.length) {
    details.push(el("p", { class: "card-meta" }, `Nested: ${skill.children.join(", ")}`));
  }
  if (skill.resources && skill.resources.length) {
    details.push(el("p", { class: "card-meta" }, `Files: ${skill.resources.join(", ")}`));
  }
  if (skill.tags && skill.tags.length) {
    details.push(el("p", { class: "card-meta" }, `Tags: ${skill.tags.join(", ")}`));
  }

  return el(
    "article",
    { class: `card${skill.universal ? " is-on" : ""}`, ...indent },
    el(
      "div",
      { class: "card-head" },
      el(
        "div",
        { class: "card-heading" },
        el("h3", { class: "card-title" }, skill.name || skill.id),
        el("p", { class: "card-meta" }, skill.id),
        skill.brief && el("p", { class: "card-desc" }, skill.brief)
      ),
      badges.length ? el("div", { class: "card-badges" }, ...badges) : null
    ),
    score,
    details.length
      ? el("details", { class: "card-more" }, el("summary", {}, "Details"), ...details)
      : null
  );
}

// --- models -----------------------------------------------------------------

/* The model catalogue, which unlike skills and tools is editable here.
 *
 * A model is a slug the provider understands, so the slug is the primary field
 * and the label is decoration. Nothing validates a slug against a provider —
 * a wrong one reports itself on the next turn — so the panel's job is to make
 * what is in the picker legible, not to police it.
 */

const SOURCE = {
  config: {
    label: "configured",
    hint: "Listed in thetis.toml. Editing it here overrides the label without touching the file.",
  },
  override: {
    label: "relabelled",
    hint: "Configured in thetis.toml, renamed here. Restore puts the file's label back.",
  },
  custom: {
    label: "added here",
    hint: "Added through this panel and kept in the grip key-value store, so it survives a restart.",
  },
};

/** The add-a-model form, and the editor for one row: the same fields either way,
 *  so adding and editing cannot drift apart. */
export function modelForm({ model, onSave, onCancel }) {
  const editing = Boolean(model);
  const slug = el("input", {
    class: "field mono",
    type: "text",
    value: model?.id || "",
    placeholder: "anthropic/claude-sonnet-4.5",
    spellcheck: "false",
    autocapitalize: "off",
    "aria-label": "Model slug",
  });
  const label = el("input", {
    class: "field",
    type: "text",
    value: (editing && model.source !== "config" && model.label) || "",
    placeholder: editing ? model.label || "Display name (optional)" : "Display name (optional)",
    "aria-label": "Display name",
  });

  const submit = () => {
    const id = slug.value.trim();
    if (!id) {
      slug.focus();
      return;
    }
    onSave({ slug: id, label: label.value.trim(), previous: model?.id || "" });
  };

  for (const field of [slug, label]) {
    field.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        submit();
      }
      if (event.key === "Escape" && onCancel) {
        event.preventDefault();
        onCancel();
      }
    });
  }

  const form = el(
    "div",
    { class: `model-form${editing ? " is-editing" : ""}` },
    el(
      "div",
      { class: "field-row" },
      el("label", { class: "field-label" }, "Slug"),
      slug
    ),
    el(
      "div",
      { class: "field-row" },
      el("label", { class: "field-label" }, "Name"),
      label
    ),
    el(
      "div",
      { class: "model-form-actions" },
      onCancel && el("button", { type: "button", class: "ghost-btn", onClick: onCancel }, "Cancel"),
      el(
        "button",
        { type: "button", class: "ghost-btn is-primary", onClick: submit },
        editing ? "Save" : "Add model"
      )
    )
  );

  // Focus the slug when adding; when editing, the slug is usually right and the
  // label is what is being changed.
  setTimeout(() => (editing ? label : slug).focus(), 0);
  return form;
}

/** One model in the catalogue. `selected` marks the one this conversation uses,
 *  because removing that is the one edit with a visible consequence. */
export function modelItem(model, { selected, onEdit, onRemove, onRestore, onUse } = {}) {
  const source = SOURCE[model.source] || SOURCE.custom;
  const actions = [];
  if (onUse && !model.hidden && !selected) {
    actions.push(
      el(
        "button",
        { type: "button", class: "ghost-btn", title: "Use this model in this conversation", onClick: () => onUse(model) },
        "Use"
      )
    );
  }
  if (onEdit) {
    actions.push(
      el("button", { type: "button", class: "ghost-btn", onClick: () => onEdit(model) }, "Edit")
    );
  }
  if (onRestore && (model.hidden || model.source === "override")) {
    actions.push(
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn",
          title: "Forget every edit to this slug",
          onClick: () => onRestore(model),
        },
        model.hidden ? "Restore" : "Reset"
      )
    );
  }
  if (onRemove && !model.hidden) {
    actions.push(
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn is-danger",
          title:
            model.source === "custom"
              ? "Forget this model"
              : "Hide this configured model from the picker",
          onClick: () => onRemove(model),
        },
        model.source === "custom" ? "Delete" : "Hide"
      )
    );
  }

  return el(
    "article",
    { class: `card${selected ? " is-on" : ""}` },
    el(
      "div",
      { class: "card-head" },
      el(
        "div",
        { class: "card-heading" },
        el("h3", { class: "card-title" }, model.label || model.id),
        el("p", { class: "card-meta" }, model.id)
      ),
      el(
        "div",
        { class: "card-badges" },
        selected ? el("span", { class: "pill pill-on" }, "in use") : null,
        el("span", { class: "pill", title: source.hint }, source.label)
      )
    ),
    actions.length ? el("div", { class: "card-actions" }, ...actions) : null
  );
}

/** One commit in the branch history view. */
export function commitItem(commit, { isHead, onReset } = {}) {
  const short = commit.rev.slice(0, 12);
  const when = commit.ts_ms ? new Date(commit.ts_ms).toLocaleString() : "";
  return el(
    "div",
    { class: "card" },
    el(
      "div",
      { class: "card-head" },
      el(
        "div",
        { class: "card-heading" },
        el("h4", { class: "card-title mono" }, short),
        el("p", { class: "card-desc" }, commit.subject)
      ),
      el(
        "div",
        { class: "card-badges" },
        isHead ? el("span", { class: "pill pill-on" }, "HEAD") : null,
        commit.on_trunk ? el("span", { class: "pill" }, "on trunk") : null
      )
    ),
    el("p", { class: "card-meta" }, [commit.author, when].filter(Boolean).join(" · ")),
    !isHead && onReset
      ? el(
          "div",
          { class: "card-actions" },
          el(
            "button",
            {
              type: "button",
              class: "ghost-btn is-danger",
              title: "Restore the branch to this commit (as a new commit; history is kept)",
              onClick: (event) => onReset(commit, event.currentTarget),
            },
            "Reset here"
          )
        )
      : null
  );
}

/** A tool: what it does, how it is provided, and its arguments.
 *
 * The description is a sibling of the head rather than a child of the heading:
 * inside it, the badge column stole a third of the width and a paragraph of
 * prose wrapped into a ragged gutter. The name and its badges share the top
 * line; the prose gets the whole card. */
export function toolItem(tool) {
  const badges = (tool.capabilities || []).map((cap) =>
    el("span", { class: `badge is-${cap.replace(/[^a-z]/gi, "-")}` }, cap)
  );

  // `attached === false` means the tool exists and is permitted in this mode,
  // but its group is not in the active set, so its definition is not in the
  // prompt. Dimming the card says that without hiding the tool: a withheld tool
  // is still callable — naming it admits its group — and hiding it would make
  // the panel a worse answer to "what can this conversation do" than the flat
  // list it replaced.
  const detached = tool.attached === false;

  return el(
    "article",
    { class: `card${detached ? " is-detached" : ""}` },
    el(
      "div",
      { class: "card-head" },
      el("div", { class: "card-heading" }, el("h3", { class: "card-title mono" }, tool.name)),
      badges.length || detached
        ? el(
            "div",
            { class: "badges" },
            detached
              ? el(
                  "span",
                  {
                    class: "badge is-detached",
                    title:
                      "Not in the prompt: this tool's group is not attached. Calling it anyway attaches the group.",
                  },
                  "not loaded"
                )
              : null,
            ...badges
          )
        : null
    ),
    tool.description ? el("p", { class: "card-desc" }, tool.description) : null,
    el(
      "details",
      { class: "card-more" },
      el("summary", {}, "Arguments"),
      el("pre", { class: "card-pre" }, formatSchema(tool.schema))
    )
  );
}

/* Why a tool group is attached, in words rather than the store's shorthand.
 *
 * These come from `REASON_*` in agents/agent-core/src/groups.rs and mean
 * nothing on their own, so each gets a plain reading and the reasoning on
 * hover — the same treatment the skills panel gives the ranker's `how`. An
 * unrecognised reason renders as itself rather than being dropped: the two
 * components share a vocabulary, not a type.
 */
const GROUP_REASON = {
  "always-on": {
    label: "always on",
    hint: "Almost every task needs this group, so it is attached without evidence and cannot be detached.",
  },
  configured: {
    label: "configured",
    hint: "Named in tool_groups.always_on in the config file, so this deployment attaches it unconditionally.",
  },
  skill: {
    label: "from a skill",
    hint: "A skill retrieved for this conversation carries a tool-group: tag pointing here. The strongest signal available.",
  },
  tag: {
    label: "word match",
    hint: "The opening message used words this group is tagged with.",
  },
  search: {
    label: "agent asked",
    hint: "The agent called tool_search and attached this group itself, mid-conversation.",
  },
  manual: {
    label: "you attached it",
    hint: "Attached by hand from this panel, overriding the routing.",
  },
};

/** The status and controls for one tool group, for `collapsibleSection`'s aside.
 *
 * @param {object} group     the published table entry: id, brief, tags, always_on
 * @param {object} opts
 * @param {boolean} opts.attached
 * @param {string} [opts.reason]
 * @param {boolean} opts.locked      always-on: shown, not offered as a switch
 * @param {(attach:boolean)=>void} opts.onToggle
 */
export function toolGroupAside(group, opts) {
  const { attached, reason, locked, onToggle } = opts;
  const nodes = [];

  if (attached && reason) {
    const meta = GROUP_REASON[reason];
    nodes.push(
      el(
        "span",
        { class: "pill", title: meta ? meta.hint : `Reason recorded as "${reason}".` },
        meta ? meta.label : reason
      )
    );
  }

  // An always-on group gets no switch, and says why instead of failing quietly
  // when pressed. `core` holds tool_search, the route by which every detached
  // group is recovered, so detaching it would remove the escape hatch itself.
  if (locked) {
    nodes.push(
      el(
        "span",
        {
          class: "pill pill-on",
          title:
            "This group is always attached. It holds the tools every task needs — including tool_search, which is how a detached group gets attached again.",
        },
        "locked on"
      )
    );
    return nodes;
  }

  nodes.push(
    el(
      "button",
      {
        type: "button",
        class: `ghost-btn sm${attached ? "" : " is-primary"}`,
        title: attached
          ? `Remove ${group.id} from the prompt. The agent can still attach it by calling one of its tools.`
          : `Add ${group.id} to the prompt for this conversation.`,
        onClick: (event) => {
          // The button lives inside a <summary>, so without this the click
          // folds the group as a side effect of pressing it.
          event.preventDefault();
          event.stopPropagation();
          onToggle(!attached);
        },
      },
      attached ? "Detach" : "Attach"
    )
  );

  return nodes;
}

/** Renders a JSON Schema as a readable argument list rather than raw JSON. */
function formatSchema(raw) {
  let schema;
  try {
    schema = JSON.parse(raw || "{}");
  } catch {
    return raw || "(none)";
  }

  const props = schema.properties || {};
  const names = Object.keys(props);
  if (!names.length) return "Takes no arguments.";

  const required = new Set(schema.required || []);
  return names
    .map((name) => {
      const spec = props[name] || {};
      const type = spec.type || "any";
      const flag = required.has(name) ? "required" : "optional";
      const note = spec.description ? `\n    ${spec.description}` : "";
      return `${name} (${type}, ${flag})${note}`;
    })
    .join("\n\n");
}
