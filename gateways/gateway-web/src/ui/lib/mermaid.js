/* Mermaid diagrams in assistant messages.
 *
 * A ```mermaid fence renders as a diagram; anything that fails to parse, or
 * fails because the library did not load, falls back to the ordinary fenced
 * code block with its copy button. The reader is therefore never worse off
 * than before this file existed, which is the whole contract here — a diagram
 * is an enhancement over the source, not a replacement that can strand it.
 *
 * On the "no dependencies" house rule: this is a deliberate, operator-approved
 * exception, the second after xterm.js. A diagram layout engine is a pile of
 * graph algorithms that is not worth reimplementing, and getting it wrong shows
 * up as an unreadable diagram. It is vendored and self-served rather than
 * pulled from a CDN, so the UI still reaches nothing but its own origin.
 *
 * On the "model output is never innerHTML" rule: mermaid's whole output *is* a
 * string of SVG, so there is no DOM-building route available. Three things keep
 * that honest, and none should be removed:
 *   - `securityLevel: "strict"`, which runs mermaid's bundled DOMPurify over
 *     the output and strips script and event handlers;
 *   - `htmlLabels: false`, so label text becomes SVG <text> rather than a
 *     foreignObject carrying arbitrary HTML;
 *   - the parse-then-render split, so malformed input never reaches the DOM.
 * The string is also mermaid's own construction, not the model's text passed
 * through — the model supplies graph source, which mermaid parses.
 */

import { el } from "./dom.js";

/* Resolved against this module's own URL, never written as `/vendor/...`.
 * The absolute form 404s under `/preview/<session>/`, which is the one place a
 * UI change is looked at before it reaches trunk: the host rewrites asset
 * references in the HTML it serves, but a URL this file builds at runtime is
 * invisible to that. Relative means the preview and the live port both work. */
const asset = (name) => new URL(`../vendor/${name}`, import.meta.url).href;

let libPromise = null; // the vendored bundle, loaded at most once

/* Loaded with a classic script tag rather than `import`, because the bundle is
 * an IIFE that ends in `globalThis["mermaid"] = ...` and has no export map.
 * Fetched as a module it would parse cleanly and define nothing.
 *
 * Lazy on purpose: the library is ~3.5 MB, and most conversations contain no
 * diagram at all. Nothing is fetched until the first mermaid fence appears.
 *
 * The timeout is not paranoia. If neither `load` nor `error` ever fires — an
 * environment that does not execute scripts, a proxy that stalls the response,
 * a 3.5 MB fetch on a bad connection — the promise never settles and every
 * diagram sits on "drawing diagram…" with its source unreachable behind the
 * placeholder. A rejection is what surfaces the code block, so a load that
 * cannot finish must become one.
 *
 * `globalThis.__mermaidLoadTimeoutMs` overrides the deadline. That exists so a
 * test can exercise the give-up path in a second rather than twenty — under
 * linkedom a script tag fires neither event, which is precisely the case this
 * timeout covers. Nothing in the app sets it. */
const LOAD_TIMEOUT_MS = 20000;

const loadTimeout = () =>
  Number(globalThis.__mermaidLoadTimeoutMs) > 0
    ? Number(globalThis.__mermaidLoadTimeoutMs)
    : LOAD_TIMEOUT_MS;

function loadLib() {
  if (libPromise) return libPromise;
  libPromise = new Promise((resolve, reject) => {
    const script = el("script", { src: asset("mermaid.js"), "data-mermaid": true });

    const deadline = loadTimeout();
    const timer = setTimeout(
      () => reject(new Error(`mermaid.js did not load within ${deadline}ms`)),
      deadline
    );
    // Settling first is harmless — a promise ignores later calls — but the
    // timer must be cleared either way or it holds the page awake.
    const settle = (fn) => (arg) => {
      clearTimeout(timer);
      fn(arg);
    };
    resolve = settle(resolve);
    reject = settle(reject);

    script.addEventListener("load", () => {
      const lib = globalThis.mermaid;
      if (!lib) return reject(new Error("mermaid.js loaded but defined no mermaid"));
      try {
        lib.initialize({
          startOnLoad: false,
          securityLevel: "strict",
          htmlLabels: false,
          flowchart: { htmlLabels: false },
          theme: "base",
          themeVariables: themeVariables(),
          fontFamily: cssValue("--font") || "sans-serif",
        });
      } catch (err) {
        return reject(err);
      }
      resolve(lib);
    });
    script.addEventListener("error", () => reject(new Error("mermaid.js did not load")));
    document.head.append(script);
  });
  return libPromise;
}

/** One custom property, resolved through the cascade. */
function cssValue(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/* The palette comes from theme.css rather than mermaid's stock dark theme, so a
 * diagram belongs to the surface around it. Mermaid wants plain colour strings
 * and cannot resolve a custom property itself; getComputedStyle hands back
 * resolved values, including for the color-mix() washes. */
function themeVariables() {
  const v = cssValue;
  const line = v("--hairline-strong") || "#32323f";
  const text = v("--text") || "#ececf2";
  return {
    darkMode: true,
    background: v("--surface-1"),
    primaryColor: v("--surface-3"),
    primaryTextColor: text,
    primaryBorderColor: v("--accent-edge"),
    secondaryColor: v("--surface-2"),
    secondaryTextColor: text,
    secondaryBorderColor: line,
    tertiaryColor: v("--surface-2"),
    tertiaryTextColor: text,
    tertiaryBorderColor: line,
    lineColor: line,
    textColor: text,
    mainBkg: v("--surface-3"),
    nodeBorder: v("--accent-edge"),
    nodeTextColor: text,
    clusterBkg: v("--surface-1"),
    clusterBorder: line,
    titleColor: text,
    edgeLabelBackground: v("--surface-1"),
    actorBkg: v("--surface-3"),
    actorBorder: v("--accent-edge"),
    actorTextColor: text,
    signalColor: text,
    signalTextColor: text,
    labelBoxBkgColor: v("--surface-3"),
    labelBoxBorderColor: line,
    labelTextColor: text,
    loopTextColor: text,
    noteBkgColor: v("--surface-2"),
    noteBorderColor: line,
    noteTextColor: text,
    errorBkgColor: v("--err-wash"),
    errorTextColor: v("--err"),
    fontSize: "13px",
  };
}

/* Rendered SVG, keyed by diagram source.
 *
 * This is not a micro-optimisation, it is what makes streaming usable.
 * `renderMarkdown` runs on every delta of an assistant message, so a completed
 * diagram is re-rendered on each of the dozens of frames that follow it. Keyed
 * by source, the second and later renders are a string lookup, which also stops
 * the diagram flickering as the text after it arrives. */
const cache = new Map(); // source -> rendered SVG string
const CACHE_MAX = 64;
let seq = 0;

/** True for the fence languages that should render as a diagram. */
export function isMermaid(lang) {
  return /^(mermaid|mmd)$/i.test(String(lang || "").trim());
}

/* Builds the node for a mermaid fence.
 *
 * Returns synchronously — `renderMarkdown` is synchronous and is called from
 * the transcript's render path — and fills itself in when the render resolves.
 * `fallback` is a thunk so the code block is only built if it is actually
 * needed. */
export function mermaidBlock(code, fallback) {
  const source = String(code || "").trim();

  const host = el("div", { class: "md-mermaid" });
  const figure = el("div", { class: "md-mermaid-figure" });
  host.append(figure);

  // A cache hit is placed immediately, so a re-render during streaming never
  // shows the "drawing" state or replaces a diagram already on screen.
  const hit = cache.get(source);
  if (hit) {
    place(host, figure, hit, source);
    return host;
  }

  figure.append(el("div", { class: "md-mermaid-wait" }, "drawing diagram…"));

  const give_up = (err) => {
    // Never leave the reader with nothing: swap in the code block, which is
    // exactly what this fence rendered as before diagrams existed.
    const block = fallback();
    block.classList.add("md-mermaid-failed");
    block.append(
      el(
        "div",
        { class: "md-mermaid-error", title: String(err?.message || err || "") },
        "This diagram could not be drawn — showing its source."
      )
    );
    host.replaceChildren(block);
  };

  (async () => {
    let lib;
    try {
      lib = await loadLib();
    } catch (err) {
      return give_up(err);
    }
    try {
      // Parse first: a syntax error thrown here never reaches the DOM, and
      // mermaid otherwise plants its own error graphic in the page.
      await lib.parse(source);
      const { svg } = await lib.render(`md-mermaid-${++seq}`, source);
      if (cache.size >= CACHE_MAX) cache.delete(cache.keys().next().value);
      cache.set(source, svg);
      place(host, figure, svg, source);
    } catch (err) {
      give_up(err);
    }
  })();

  return host;
}

/* Puts the SVG in place, with the source available behind a copy button.
 *
 * See the header note on innerHTML: the string is mermaid's own output, passed
 * through its bundled DOMPurify by securityLevel "strict". */
function place(host, figure, svg, source) {
  figure.replaceChildren();
  figure.innerHTML = svg;

  const node = figure.querySelector("svg");
  if (node) {
    // Mermaid fixes a width in px and often a max-width in a style attribute;
    // both stop the diagram fitting a narrow column. Scale to the column and
    // keep the aspect ratio from the viewBox.
    node.removeAttribute("width");
    node.removeAttribute("height");
    node.style.maxWidth = "100%";
    node.style.height = "auto";
    node.setAttribute("role", "img");
  }

  const copy = el(
    "button",
    {
      type: "button",
      class: "md-copy",
      title: "Copy this diagram's source",
      onClick: () => {
        navigator.clipboard?.writeText(source).then(
          () => flash(copy, "copied"),
          () => flash(copy, "copy failed")
        );
      },
    },
    "Copy source"
  );

  host.replaceChildren(
    el("div", { class: "md-code-head" }, el("span", { class: "md-code-lang" }, "mermaid"), copy),
    figure
  );
}

function flash(button, text) {
  const previous = button.textContent;
  button.textContent = text;
  setTimeout(() => (button.textContent = previous), 1200);
}
