/* A small markdown renderer for assistant messages.
 *
 * Deliberately hand-rolled and DOM-built: the UI is dependency-free, and
 * building nodes (never innerHTML from model output) is what makes rendering
 * model text safe. Covers what the agent actually writes — paragraphs,
 * headings, fenced code with a copy button, inline code, bold, italic, links,
 * flat lists, blockquotes and rules. Anything fancier renders as plain text,
 * which is exactly what the old transcript did for everything.
 */

import { el } from "./dom.js";

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
      blocks.push(codeBlock(body.join("\n"), fence[1]));
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
