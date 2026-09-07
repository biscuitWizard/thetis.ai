/* A small markdown renderer for assistant messages.
 *
 * Deliberately hand-rolled and DOM-built: the UI is dependency-free, and
 * building nodes (never innerHTML from model output) is what makes rendering
 * model text safe. Covers what the agent actually writes — paragraphs,
 * headings, fenced code with a copy button, inline code, bold, italic, links,
 * flat lists, blockquotes, rules and pipe tables. Anything fancier renders as
 * plain text, which is exactly what the old transcript did for everything.
 *
 * A ```mermaid fence is the one exception to the DOM-building rule; see
 * `mermaid.js` for what keeps that safe and why it is worth it.
 */

import { el } from "./dom.js";
import { isMermaid, mermaidBlock } from "./mermaid.js";

/** Renders markdown to an array of block nodes. */
export function renderMarkdown(text) {
  const lines = String(text ?? "").split("\n");
  const blocks = [];
  let paragraph = [];
  let list = null; // { ordered, items: [] }

  const flushParagraph = () => {
    if (!paragraph.length) return;
    blocks.push(el("p", { class: "md-p" }, ...inline(paragraph.join("\n"))));
    paragraph = [];
  };
  const flushList = () => {
    if (!list) return;
    blocks.push(
      el(
        list.ordered ? "ol" : "ul",
        { class: "md-list" },
        list.items.map((item) => el("li", {}, ...inline(item)))
      )
    );
    list = null;
  };
  const flush = () => {
    flushParagraph();
    flushList();
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // Fenced code: swallow lines until the closing fence (or the end).
    const fence = line.match(/^```(\S*)\s*$/);
    if (fence) {
      flush();
      const body = [];
      while (++i < lines.length && !/^```\s*$/.test(lines[i])) body.push(lines[i]);
      const code = body.join("\n");
      const lang = fence[1];
      // A mermaid fence draws a diagram, and falls back to exactly this code
      // block if the library or the source will not cooperate.
      blocks.push(
        isMermaid(lang) ? mermaidBlock(code, () => codeBlock(code, lang)) : codeBlock(code, lang)
      );
      continue;
    }

    // Pipe tables. Checked before headings and the paragraph fallthrough, so a
    // table interrupts whatever came before it.
    const table = tableAt(lines, i);
    if (table) {
      flush();
      blocks.push(table.node);
      i += table.consumed;
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      flush();
      const level = Math.min(heading[1].length + 2, 6); // h3..h6: chat text, not a document
      blocks.push(el(`h${level}`, { class: "md-h" }, ...inline(heading[2])));
      continue;
    }

    if (/^(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
      flush();
      blocks.push(el("hr", { class: "md-hr" }));
      continue;
    }

    const quoted = line.match(/^>\s?(.*)$/);
    if (quoted) {
      flush();
      // Consecutive quote lines fold into one block.
      const body = [quoted[1]];
      while (i + 1 < lines.length && /^>\s?/.test(lines[i + 1])) {
        body.push(lines[++i].replace(/^>\s?/, ""));
      }
      blocks.push(el("blockquote", { class: "md-quote" }, ...inline(body.join("\n"))));
      continue;
    }

    const bullet = line.match(/^\s*[-*+]\s+(.*)$/);
    const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (bullet || numbered) {
      flushParagraph();
      const ordered = Boolean(numbered);
      if (!list || list.ordered !== ordered) {
        flushList();
        list = { ordered, items: [] };
      }
      list.items.push((bullet || numbered)[1]);
      continue;
    }

    if (!line.trim()) {
      flush();
      continue;
    }

    // A list ends at the first non-item line.
    flushList();
    paragraph.push(line);
  }

  flush();
  return blocks;
}

/** A fenced block: language strip, copy button, and the code itself. */
function codeBlock(code, lang) {
  const button = el(
    "button",
    {
      type: "button",
      class: "md-copy",
      title: "Copy this block",
      onClick: () => {
        navigator.clipboard?.writeText(code).then(
          () => flash(button, "copied"),
          () => flash(button, "copy failed")
        );
      },
    },
    "Copy"
  );
  return el(
    "div",
    { class: "md-code" },
    el("div", { class: "md-code-head" }, el("span", { class: "md-code-lang" }, lang || "text"), button),
    el("pre", {}, el("code", {}, code))
  );
}

/* --- tables -----------------------------------------------------------------
 *
 * GFM pipe tables: a header row, a delimiter row that sets each column's
 * alignment, then body rows until the first line without a pipe.
 *
 * Also handles the *collapsed* form, where a whole table arrives on a single
 * line because it came through something that ate the newlines:
 * `| a | b | |---|---| | 1 | 2 |`. That form is genuinely ambiguous — an empty
 * leading cell and a row boundary are both spelled `| |` — so it is resolved by
 * counting cells against the header's width rather than by guessing. See
 * `splitCollapsed` and `chunkRows`.
 */

const DELIM_CELL = /^:?-+:?$/;

/** Reads a table starting at `lines[start]`, or null if there is not one. */
function tableAt(lines, start) {
  const first = lines[start];
  if (!first || !first.includes("|")) return null;

  // The ordinary multi-line table.
  const next = lines[start + 1];
  if (next && next.includes("|") && splitCells(next).every((c) => DELIM_CELL.test(c.trim()))) {
    const header = splitCells(first);
    const align = splitCells(next).map(alignOf);
    const rows = [];
    let i = start + 2;
    while (i < lines.length && lines[i].trim() && lines[i].includes("|")) {
      rows.push(splitCells(lines[i]));
      i++;
    }
    // `consumed` is how far to advance beyond `start`; the loop's own `i++`
    // moves onto the first line this table did not claim.
    return { node: tableNode(header, align, rows), consumed: i - start - 1 };
  }

  const collapsed = splitCollapsed(first);
  return collapsed ? { node: tableNode(collapsed.header, collapsed.align, collapsed.rows), consumed: 0 } : null;
}

/* A table collapsed onto one line. The delimiter cells are the one unambiguous
 * landmark, so they anchor everything: what precedes them is the header, what
 * follows is the body. */
function splitCollapsed(line) {
  const tokens = splitCells(line);
  const runStart = tokens.findIndex((t) => DELIM_CELL.test(t.trim()));
  if (runStart <= 0) return null;
  let runEnd = runStart;
  while (runEnd + 1 < tokens.length && DELIM_CELL.test(tokens[runEnd + 1].trim())) runEnd++;
  // Two dash cells before this is treated as a table: one is far more likely to
  // be a paragraph that happens to contain a pipe and a dash.
  if (runEnd === runStart) return null;

  const header = tokens.slice(0, runStart);
  // The header's last token is the pad where its closing pipe met the delimiter
  // row's opening one. A header genuinely ending in an empty cell is rarer than
  // that boundary, which is always present.
  if (header.length > 1 && !header[header.length - 1].trim()) header.pop();

  return {
    header,
    align: tokens.slice(runStart, runEnd + 1).map(alignOf),
    rows: chunkRows(tokens.slice(runEnd + 1), header.length),
  };
}

/* Cuts a flat run of collapsed body cells into rows of `width`.
 *
 * Each row may be preceded by one blank token — the `| |` where a row's closing
 * pipe meets the next row's opening pipe — and it is indistinguishable from an
 * empty first cell by inspection. Arithmetic settles it: if the count divides
 * evenly by `width` there are no pads, and if it instead divides by `width + 1`
 * with every stride-th token blank, there is exactly one pad per row. */
function chunkRows(cells, width) {
  if (!width || !cells.length) return [];
  const padded =
    cells.length % width !== 0 &&
    cells.length % (width + 1) === 0 &&
    blankEveryStride(cells, width + 1);
  const stride = padded ? width + 1 : width;

  const rows = [];
  for (let i = 0; i < cells.length; i += stride) {
    const row = cells.slice(i, i + stride);
    rows.push(padded ? row.slice(1) : row);
  }
  return rows;
}

function blankEveryStride(cells, stride) {
  for (let i = 0; i < cells.length; i += stride) if (cells[i].trim()) return false;
  return true;
}

/* Splits one row into cells: outer pipes dropped, `\|` kept as a literal. */
function splitCells(line) {
  let text = line.trim();
  if (text.startsWith("|")) text = text.slice(1);
  if (text.endsWith("|") && !text.endsWith("\\|")) text = text.slice(0, -1);

  const cells = [];
  let cell = "";
  for (let i = 0; i < text.length; i++) {
    if (text[i] === "\\" && text[i + 1] === "|") {
      cell += "|";
      i++;
    } else if (text[i] === "|") {
      cells.push(cell);
      cell = "";
    } else {
      cell += text[i];
    }
  }
  cells.push(cell);
  return cells;
}

/** `:---` left, `---:` right, `:---:` centre, plain: no opinion. */
function alignOf(cell) {
  const text = cell.trim();
  const left = text.startsWith(":");
  const right = text.endsWith(":");
  if (left && right) return "center";
  if (right) return "right";
  if (left) return "left";
  return null;
}

function tableNode(header, align, rows) {
  const width = header.length;
  const cell = (tag, text, column) =>
    el(tag, align[column] ? { class: `md-cell-${align[column]}` } : {}, ...inline(text.trim()));

  // Wrapped, because a table wider than the 48rem measure has to scroll on its
  // own rather than stretch the text column around it.
  return el(
    "div",
    { class: "md-table-wrap" },
    el(
      "table",
      { class: "md-table" },
      el("thead", {}, el("tr", {}, header.map((text, n) => cell("th", text, n)))),
      el(
        "tbody",
        {},
        rows.map((row) => el("tr", {}, fit(row, width).map((text, n) => cell("td", text, n))))
      )
    )
  );
}

/** A ragged row is padded or truncated to the header's width. */
function fit(row, width) {
  const cells = row.slice(0, width);
  while (cells.length < width) cells.push("");
  return cells;
}

function flash(button, text) {
  const previous = button.textContent;
  button.textContent = text;
  setTimeout(() => (button.textContent = previous), 1200);
}

/* Inline spans: `code`, **bold**, *italic*, [text](http…). One pass, earliest
 * match first, so constructs cannot nest — which is the honest amount of
 * markdown for chat text. */
const INLINE = [
  { re: /`([^`\n]+)`/, node: (m) => el("code", { class: "md-inline-code" }, m[1]) },
  { re: /\*\*([^*\n]+)\*\*/, node: (m) => el("strong", {}, m[1]) },
  { re: /\*([^*\n]+)\*/, node: (m) => el("em", {}, m[1]) },
  {
    re: /\[([^\]\n]+)\]\((https?:\/\/[^)\s]+)\)/,
    node: (m) => el("a", { href: m[2], target: "_blank", rel: "noopener noreferrer" }, m[1]),
  },
];

function inline(text) {
  const nodes = [];
  let rest = text;
  while (rest) {
    let best = null;
    for (const spec of INLINE) {
      const match = spec.re.exec(rest);
      if (match && (!best || match.index < best.match.index)) {
        best = { spec, match };
      }
    }
    if (!best) {
      nodes.push(rest);
      break;
    }
    if (best.match.index > 0) nodes.push(rest.slice(0, best.match.index));
    nodes.push(best.spec.node(best.match));
    rest = rest.slice(best.match.index + best.match[0].length);
  }
  return nodes;
}
