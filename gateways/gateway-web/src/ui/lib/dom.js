/* Tiny DOM helpers. No framework, no build step. */

export const $ = (id) => document.getElementById(id);

/** Creates an element. Children may be nodes or strings (inserted as text). */
export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);

  for (const [key, value] of Object.entries(props)) {
    if (value == null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "html") node.innerHTML = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2).toLowerCase(), value);
    else if (key === "dataset") Object.assign(node.dataset, value);
    else node.setAttribute(key, value === true ? "" : value);
  }

  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

/** Inline SVG from a path spec, so icons stay markup rather than image loads. */
export function icon(paths, { size = 16, fill = "none", width = 1.7 } = {}) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 20 20");
  svg.setAttribute("width", size);
  svg.setAttribute("height", size);
  svg.setAttribute("aria-hidden", "true");

  for (const d of [].concat(paths)) {
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", d);
    path.setAttribute("fill", fill);
    if (fill === "none") {
      path.setAttribute("stroke", "currentColor");
      path.setAttribute("stroke-width", width);
      path.setAttribute("stroke-linecap", "round");
      path.setAttribute("stroke-linejoin", "round");
    }
    svg.append(path);
  }
  return svg;
}

export function clear(node) {
  node.replaceChildren();
  return node;
}

/** Closes a menu when the next click lands outside it. */
export function onClickOutside(node, handler) {
  const listener = (event) => {
    if (!node.contains(event.target)) handler(event);
  };
  // Deferred so the click that opened the menu does not immediately close it.
  setTimeout(() => document.addEventListener("click", listener), 0);
  return () => document.removeEventListener("click", listener);
}
