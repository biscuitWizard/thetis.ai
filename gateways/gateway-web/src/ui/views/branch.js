/* The branch view: a live commit graph of this conversation's sandbox.
 *
 * Lives in the rail's Branch tab — its default tab, so the graph stays
 * ever-present the way the old sidebar dock was, with room to breathe. Two
 * rails, drawn from the real topology: trunk on the right, this conversation's
 * branch on the left, forking at its base commit. The graph is contextual —
 * before the first message it shows trunk with a ghost fork (click a trunk
 * node to start from there); afterwards update-merges draw cross edges and a
 * dashed hint shows the pending merge whenever the branch is ahead. Clicking
 * a commit opens History, where reset lives behind its own two-step control —
 * a destructive verb has no business on a bare graph node.
 *
 * Hand-rolled SVG: the UI is dependency-free by design, and two lanes with
 * cross edges is the entire topology a conversation can have.
 */

import { el } from "../lib/dom.js";
import { store, denied } from "../lib/store.js";

const ROW = 22; // px per commit row
const LANE_BRANCH = 14;
const LANE_TRUNK = 38;
const LABEL_X = 50;

const STATE_DOTS = {
  clean: "is-ok",
  dirty: "is-warn",
  idle: "is-ok",
  conflict: "is-err",
};

function svgEl(tag, attrs) {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value != null) node.setAttribute(key, value);
  }
  return node;
}

export function mountBranch({ onMerge, onUpdate, onHistory, onResolve, onAbort, onPickBase, onChange }) {
  /** The whole Branch tab body, or null when there is nothing to show. */
  function render() {
    const graph = store.branchGraph;
    const branch = store.branch;
    if (!store.current || !graph) return null;

    const materialized = Boolean(branch?.materialized && graph.branch_name);
    const conflicted = branch?.state === "conflict";
    // A role may look at the branch and not change it. The host refuses the
    // mutating frames regardless; leaving the buttons out is so the tab does
    // not offer what it will then refuse.
    const canWrite = !denied("branch_write");
    const withheld = () => el("div", { class: "branch-note" }, "Branch changes are withheld by your role.");
    const parts = [];

    // --- header -----------------------------------------------------------
    const title = materialized ? graph.branch_name : `${graph.trunk_name} (trunk)`;
    parts.push(
      el(
        "div",
        { class: "branch-head" },
        el("span", { class: `branch-dot ${STATE_DOTS[branch?.state] || "is-ok"}` }),
        el("span", { class: "branch-name", title }, title),
        branch?.state === "dirty"
          ? el("span", { class: "branch-pill is-warn", title: "Uncommitted changes in the sandbox" }, "editing")
          : null,
        materialized && branch?.ahead > 0
          ? el("span", { class: "branch-pill is-ahead", title: `${branch.ahead} commit(s) trunk lacks` }, `↑${branch.ahead}`)
          : null,
        materialized && branch?.behind > 0
          ? el("span", { class: "branch-pill is-behind", title: `trunk has ${branch.behind} commit(s) this branch lacks` }, `↓${branch.behind}`)
          : null
      )
    );

    // --- conflict card / actions ------------------------------------------
    if (conflicted) {
      const files = branch.conflicts || [];
      parts.push(
        el(
          "div",
          { class: "branch-conflict" },
          el("div", { class: "branch-conflict-head" }, `Merge conflict — ${files.length} file${files.length === 1 ? "" : "s"}`),
          ...files.slice(0, 4).map((f) => el("div", { class: "branch-conflict-file", title: f }, f)),
          files.length > 4 ? el("div", { class: "branch-note" }, `…and ${files.length - 4} more`) : null,
          canWrite
            ? el(
                "div",
                { class: "branch-actions" },
                el("button", { type: "button", class: "ghost-btn is-primary",
                  onClick: (event) => onResolve(event.currentTarget),
                  title: "Hand the conflict to this conversation; the agent resolves it and completes the merge" },
                  "Resolve in conversation"),
                el("button", { type: "button", class: "ghost-btn is-danger",
                  onClick: (event) => onAbort(event.currentTarget),
                  title: "Abandon the merge and restore the pre-merge state" }, "Abort")
              )
            : withheld()
        )
      );
    } else if (!materialized) {
      parts.push(
        el("div", { class: "branch-note" },
          "This conversation will fork from trunk at its first message — click a commit to start from there instead.")
      );
    } else if (!canWrite) {
      const actions = el("div", { class: "branch-actions" });
      actions.append(
        el("button", { type: "button", class: "ghost-btn", title: "The full history, with details", onClick: onHistory }, "History…")
      );
      parts.push(withheld(), actions);
    } else {
      const actions = el("div", { class: "branch-actions" });
      const mergeReady = branch.ahead > 0 && branch.behind === 0;
      actions.append(
        el("button", {
          type: "button",
          class: `ghost-btn${mergeReady ? " is-primary" : ""}`,
          disabled: branch.ahead === 0 || store.busy ? "disabled" : undefined,
          title: branch.ahead === 0
            ? "Nothing here that trunk lacks"
            : "Land this conversation's changes on trunk, for every new conversation to inherit",
          onClick: (event) => onMerge(event.currentTarget),
        }, "Merge → trunk"),
        el("button", {
          type: "button",
          class: "ghost-btn",
          disabled: branch.behind === 0 || store.busy ? "disabled" : undefined,
          title: branch.behind === 0
            ? "Trunk has nothing this branch lacks"
            : "Bring the latest trunk into this conversation",
          onClick: (event) => onUpdate(event.currentTarget),
        }, "Update from trunk"),
        el("button", { type: "button", class: "ghost-btn", title: "The full history, with details", onClick: onHistory }, "History…")
      );
      parts.push(actions);
    }

    parts.push(renderGraph(graph, { materialized, conflicted }));
    return el("div", { class: "branch-panel" }, ...parts);
  }

  /* Lay the two rails out.
   *
   * Rows, newest at top: the branch's own commits first, then trunk from its
   * head downward. The branch rail descends and curves into the base node on
   * the trunk rail — the fork, drawn where it actually happened.
   */
  function renderGraph(graph, { materialized, conflicted }) {
    const branchCommits = materialized ? graph.branch || [] : [];
    const trunkCommits = graph.trunk || [];
    const base = graph.base;
    const trunkOnly = trunkCommits.filter((c) => !branchCommits.some((b) => b.rev === c.rev));

    // A ghost row stands in for the not-yet-existing branch before the fork.
    const ghost = !materialized;
    const rows = [];
    if (ghost) rows.push({ ghost: true });
    for (const c of branchCommits) rows.push({ commit: c, lane: "branch" });
    for (const c of trunkOnly) rows.push({ commit: c, lane: "trunk" });

    const baseRow = rows.findIndex((r) => r.commit?.rev === base);
    const height = rows.length * ROW + 8;
    const svg = svgEl("svg", {
      class: `branch-graph${conflicted ? " is-conflict" : ""}`,
      width: "100%",
      height,
      viewBox: `0 0 300 ${height}`,
      preserveAspectRatio: "xMinYMin meet",
    });

    const y = (row) => row * ROW + ROW / 2 + 4;
    const firstTrunkRow = rows.findIndex((r) => r.lane === "trunk");
    const lastBranchRow = ghost ? 0 : branchCommits.length - (ghost ? 0 : 1) + (ghost ? 1 : 0);

    // Trunk rail: full height of its section.
    if (firstTrunkRow >= 0) {
      svg.append(svgEl("line", {
        class: "rail rail-trunk",
        x1: LANE_TRUNK, x2: LANE_TRUNK,
        y1: y(firstTrunkRow), y2: y(rows.length - 1),
      }));
    }

    // Branch rail: through its own commits, then a curve into the base node
    // (or into the trunk head row for the ghost fork).
    const hasBranchLane = ghost || branchCommits.length > 0;
    if (hasBranchLane) {
      const laneTop = y(0);
      const laneBottom = y(ghost ? 0 : Math.max(lastBranchRow, 0));
      if (laneBottom > laneTop) {
        svg.append(svgEl("line", {
          class: `rail rail-branch${ghost ? " is-ghost" : ""}`,
          x1: LANE_BRANCH, x2: LANE_BRANCH, y1: laneTop, y2: laneBottom,
        }));
      }
      // Where the fork lands. With a real branch it is the merge-base row.
      // Before the branch exists there is no base yet, so it is the commit
      // this conversation would start from: the one clicked, else trunk's
      // head. Falling through to the last row drew the fork at the oldest
      // visible commit, which is not where new work begins.
      const chosenRow = ghost && store.baseRevision
        ? rows.findIndex((r) => r.commit?.rev === store.baseRevision)
        : -1;
      const forkRow =
        baseRow >= 0 ? baseRow
        : chosenRow >= 0 ? chosenRow
        : ghost && firstTrunkRow >= 0 ? firstTrunkRow
        : rows.length - 1;
      const forkY = y(forkRow);
      // Ease the S-curve to the actual gap: with a one-row hop, control points
      // a full ROW out overshoot and the curve doubles back on itself.
      const bend = Math.min(ROW, Math.max(4, (forkY - laneBottom) / 2));
      svg.append(svgEl("path", {
        class: `rail rail-branch${ghost ? " is-ghost" : ""}`,
        d: `M ${LANE_BRANCH} ${laneBottom} C ${LANE_BRANCH} ${laneBottom + bend}, ${LANE_TRUNK} ${forkY - bend}, ${LANE_TRUNK} ${forkY}`,
        fill: "none",
      }));
    }

    // The pending merge, as a hint: branch head curving up toward trunk head.
    if (materialized && store.branch?.ahead > 0 && firstTrunkRow >= 0 && branchCommits.length) {
      svg.append(svgEl("path", {
        class: "rail rail-merge-hint",
        d: `M ${LANE_BRANCH} ${y(0)} C ${LANE_BRANCH} ${y(0) - ROW}, ${LANE_TRUNK} ${y(firstTrunkRow) - ROW * 0.9}, ${LANE_TRUNK} ${y(firstTrunkRow)}`,
        fill: "none",
      }));
    }

    // Cross edges: an update-merge is a branch commit with a parent on trunk.
    const trunkRevs = new Map(rows.map((r, i) => [r.commit?.rev, i]).filter(([rev]) => rev));
    rows.forEach((row, i) => {
      if (row.lane !== "branch" || !row.commit?.parents) return;
      for (const parent of row.commit.parents.slice(1)) {
        const target = trunkRevs.get(parent);
        if (target != null && rows[target].lane === "trunk") {
          svg.append(svgEl("path", {
            class: "rail rail-cross",
            d: `M ${LANE_TRUNK} ${y(target)} C ${LANE_TRUNK} ${y(target) - ROW}, ${LANE_BRANCH} ${y(i) + ROW}, ${LANE_BRANCH} ${y(i)}`,
            fill: "none",
          }));
        }
      }
    });

    // Nodes and labels.
    rows.forEach((row, i) => {
      const cy = y(i);
      if (row.ghost) {
        svg.append(svgEl("circle", { class: "node is-ghost", cx: LANE_BRANCH, cy, r: 4.5 }));
        const label = svgEl("text", { class: "graph-label is-ghost", x: LABEL_X, y: cy + 3.5 });
        label.textContent = "your work starts here";
        svg.append(label);
        return;
      }
      const c = row.commit;
      const isBase = c.rev === base;
      const isBranchHead = row.lane === "branch" && i === (ghost ? 1 : 0);
      const isTrunkHead = i === firstTrunkRow;
      const cx = row.lane === "branch" ? LANE_BRANCH : LANE_TRUNK;

      const group = svgEl("g", { class: "graph-row" });
      const hit = svgEl("rect", { class: "graph-hit", x: 0, y: cy - ROW / 2, width: 300, height: ROW });
      group.append(hit);

      const isChosenBase = !materialized && store.baseRevision && c.rev === store.baseRevision;
      const defaultHead = !materialized && !store.baseRevision && isTrunkHead;
      const node = svgEl("circle", {
        class: `node ${row.lane === "branch" ? "is-branch" : "is-trunk"}${isBase || isChosenBase ? " is-base" : ""}${isBranchHead || isChosenBase || defaultHead ? " is-head" : ""}`,
        cx, cy, r: isBase || isBranchHead || isChosenBase ? 5 : 4,
      });
      group.append(node);

      const label = svgEl("text", { class: `graph-label${row.lane === "trunk" ? " is-trunk" : ""}`, x: LABEL_X, y: cy + 3.5 });
      const subject = c.subject.length > 38 ? `${c.subject.slice(0, 37)}…` : c.subject;
      label.textContent = subject;
      group.append(label);

      const tip = svgEl("title", {});
      tip.textContent = `${c.rev.slice(0, 12)} — ${c.subject}\n${c.author}${isBase ? "\n(fork point)" : ""}`;
      group.append(tip);

      // Before the branch exists a click picks the starting point; afterwards
      // every commit opens History. Reset lives there, behind its own
      // confirming control, never on a bare 5px node.
      group.classList.add("is-clickable");
      if (!materialized) {
        group.addEventListener("click", (event) => onPickBase(c.rev, c.subject, event.currentTarget));
      } else {
        group.addEventListener("click", onHistory);
      }
      svg.append(group);
    });

    return svg;
  }

  const changed = () => onChange();
  store.watch("branch", changed);
  store.watch("branchGraph", changed);
  store.watch("busy", changed);
  store.watch("baseRevision", changed);

  return { render };
}
