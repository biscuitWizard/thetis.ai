/* @-mentions in the composer: attaching shared-workspace files by typing.
 *
 * Typing `@` opens a menu of files and folders in the shared workspace, matched
 * against what is typed after it. Picking one leaves `@path` in the message,
 * highlighted like a link, and attaches the file's *contents* to the message
 * when it is sent.
 *
 * Four decisions worth keeping:
 *
 * 1. **Matching is host-side.** The shared workspace here is tens of thousands
 *    of files across several checkouts, so indexing it in the browser was
 *    thousands of `workspace-list` round trips and still had to truncate. The
 *    host answers one `workspace-find` frame from a cached walk that every tab
 *    shares, and it knows to skip `target/`, `node_modules/` and `.git/` —
 *    which is the difference between a useful menu and forty thousand build
 *    artifacts.
 *
 * 2. **The message text is the only source of truth.** Nothing tracks "picked"
 *    items as a separate list; the attachments are re-derived by scanning the
 *    text every time. So deleting a token detaches the file, retyping it
 *    re-attaches it, and there is no way for a chip tray and the text to
 *    disagree — which is the bug every mention implementation that keeps two
 *    lists eventually grows. What is cached is only which paths are known to be
 *    real (`resolved`) and the bytes already fetched.
 *
 * 3. **Contents, not just the path.** Cursor and its relatives inline the whole
 *    file into the prompt, and that is what people expect from `@`: a path
 *    alone tells the model a file exists and leaves it to spend a tool call
 *    fetching what the sender already knew was relevant. The inlining happens
 *    in the agent (`user_content` in agent-core), which fences any text
 *    attachment and labels it with its path. Here we only carry the bytes, and
 *    the name we give an attachment is the path the agent's own file tools
 *    take, so a truncated file can be read the rest of the way.
 *
 * 4. **Bytes come over HTTP.** `GET /workspace/file/…` is the route the
 *    explorer already previews images with. Base64 in a JSON frame would cost a
 *    third more bytes and be buffered twice.
 */

import { AGENT_NAME, clear, el, icon } from "../lib/dom.js";
import { toast } from "../lib/toast.js";
import { rawUrl } from "./workspace.js";

/** Largest file attached by mention. The host takes 8 MB per attachment, but
 *  inlining megabytes of text into a prompt is never what was meant. */
const MAX_MENTION_BYTES = 1_500_000;
/** Mentions on one message, under the host's `limits.max_attachments`. */
const MAX_MENTIONS = 16;
/** Keystrokes settle for this long before a search frame goes out. */
const DEBOUNCE_MS = 90;
/** A search that never comes back must not leave the menu saying "searching…". */
const SEARCH_TIMEOUT_MS = 8000;

const ICONS = {
  dir: ["M2.5 5.5a1 1 0 0 1 1-1h4l1.6 1.8h7.4a1 1 0 0 1 1 1v8.2a1 1 0 0 1-1 1h-13a1 1 0 0 1-1-1z"],
  file: ["M5 3.5h7l3 3v10H5z", "M12 3.5v3h3"],
};

/** A mention token: `@` at a word boundary, then anything but whitespace.
 *  Paths carry slashes and dots, so the token ends only at whitespace — a
 *  trailing `,` or `)` is dealt with by `trimTail`, since punctuation there is
 *  far likelier than a filename ending in it. */
const TOKEN = /(^|[\s(\[])@([^\s@]+)/g;
/** The token being typed, anchored at the caret. */
const AT_CARET = /(?:^|[\s(\[])@([^\s@]*)$/;

function trimTail(raw) {
  return raw.replace(/[.,;:!?)\]}'"]+$/, "");
}

export function mountMentions({ input, mirror, send, onTextChange }) {
  /* --- what is known to be real -------------------------------------------- */

  /* Paths the host has confirmed exist, `path` → {path, is_dir, size}. Grown by
   * every search answer and every pick; never trusted as complete, so a token
   * not in here is simply not highlighted yet rather than declared wrong. */
  const resolved = new Map();

  function remember(entry) {
    resolved.set(entry.path, entry);
    // A folder is spelled with a trailing slash in the message, so both forms
    // resolve to the same entry.
    if (entry.is_dir) resolved.set(`${entry.path}/`, entry);
  }

  function lookup(token) {
    return resolved.get(token) || resolved.get(token.replace(/\/+$/, "")) || null;
  }

  /* --- searching ----------------------------------------------------------- */

  /** The query whose answer we are still waiting for, or null. */
  let inflight = null;
  let debounce = null;
  let timeout = null;

  function search(query) {
    clearTimeout(debounce);
    debounce = setTimeout(() => {
      // A query typed inside a folder prefix is searched within it, so
      // `@moor/cli` narrows rather than re-matching the whole tree. The prefix
      // is the part up to the last slash; the rest is the fuzzy query.
      const cut = query.lastIndexOf("/");
      const dir = cut < 0 ? "" : query.slice(0, cut);
      const rest = cut < 0 ? query : query.slice(cut + 1);
      // The full query is still sent when there is no prefix match to be had:
      // a `dir` that does not exist would return nothing at all, so the host
      // falls back on its own by matching whole paths too.
      inflight = query;
      if (!send({ type: "workspace-find", query: rest, dir })) {
        inflight = null;
        state.loading = false;
        state.note = "Not connected — cannot search the workspace.";
        drawMenu();
        return;
      }
      clearTimeout(timeout);
      timeout = setTimeout(() => {
        if (inflight !== query) return;
        inflight = null;
        state.loading = false;
        state.note = "The workspace search did not answer. Try again.";
        drawMenu();
      }, SEARCH_TIMEOUT_MS);
    }, DEBOUNCE_MS);
  }

  /** A `workspace-find` answer. Frames for a query already typed past are
   *  dropped: the echoed `query` is what makes that possible. */
  function onFind(frame) {
    const answered = joinQuery(frame.dir, frame.query);
    if (inflight !== null && answered !== inflight) return;
    inflight = null;
    clearTimeout(timeout);

    for (const entry of frame.entries || []) remember(entry);

    if (!state.open) {
      // The menu closed while the search ran, but the paths are still worth
      // keeping: a token typed by hand now highlights.
      paint();
      return;
    }
    state.items = frame.entries || [];
    state.total = frame.total || 0;
    state.active = 0;
    state.loading = false;
    state.note = null;
    drawMenu();
    paint();
  }

  function joinQuery(dir, query) {
    return dir ? `${dir}/${query ?? ""}` : (query ?? "");
  }

  /** Wired from app.js. A listing the Files tab asked for is free knowledge
   *  about which paths exist, so mentions learn from it too. */
  function onList(frame) {
    for (const entry of frame.entries || []) remember(entry);
    // The listing also proves the listed directory itself exists — which the
    // entries alone do not say, since they name only its children. Without
    // this, `@moor/crates/` could never resolve from a listing of itself.
    const path = String(frame.path ?? "").replace(/\/+$/, "");
    if (path && (frame.entries || frame.ok !== false)) {
      remember({ path, name: path.split("/").pop(), is_dir: true, size: 0 });
    }
  }

  /** A mutation happened, so a cached path may be gone. The host drops its own
   *  index; here we only need to forget bytes, since a stale highlight on a
   *  deleted file is corrected the moment it is searched or sent. */
  function onResult(frame) {
    if (frame?.ok) {
      bytes.clear();
      // A path that did not exist a moment ago may exist now, so a token
      // already written off must get another chance.
      missing.clear();
    }
  }

  /** Marks everything stale: the agent has been working in the tree. */
  function invalidate() {
    bytes.clear();
    missing.clear();
  }

  /* --- reading bytes ------------------------------------------------------- */

  /** Fetched attachments by path, so re-scanning the text costs nothing. */
  const bytes = new Map();

  function base64(buffer) {
    const view = new Uint8Array(buffer);
    let out = "";
    // Chunked: one apply() over a megabyte overflows the argument list.
    for (let i = 0; i < view.length; i += 8192) {
      out += String.fromCharCode(...view.subarray(i, i + 8192));
    }
    return btoa(out);
  }

  /** One file as an attachment, or null with a toast explaining why not. */
  async function fetchFile(entry) {
    if (entry.size > MAX_MENTION_BYTES) {
      toast(
        `${entry.path} is ${Math.round(entry.size / 1024)} KB — too large to attach. ` +
          `Name the path in words instead and ${AGENT_NAME} can read the part it needs.`,
        { tone: "error" }
      );
      return null;
    }
    try {
      const res = await fetch(rawUrl(entry.path));
      if (!res.ok) throw new Error((await res.text()) || res.statusText);
      const buffer = await res.arrayBuffer();
      return {
        // The path spelled as the agent's own file tools take it, so a
        // truncated attachment can be read the rest of the way.
        name: `workspace/${entry.path}`,
        // The host decided the type from the extension; trusting its header
        // keeps one table rather than two.
        mime: (res.headers.get("content-type") || "text/plain").split(";")[0].trim(),
        data: base64(buffer),
      };
    } catch (e) {
      toast(`Could not read ${entry.path}: ${e.message}`, { tone: "error" });
      return null;
    }
  }

  /** A folder as a listing rather than as its contents. Attaching every file
   *  under a directory would blow the context on the first mention; the shape
   *  of it plus sizes is what makes a folder mention useful, and the agent
   *  reads what it then wants. */
  async function fetchDir(entry) {
    return new Promise((resolve) => {
      const path = entry.path;
      dirWaiters.set(path, resolve);
      if (!send({ type: "workspace-list", path })) {
        dirWaiters.delete(path);
        resolve(null);
        return;
      }
      setTimeout(() => {
        if (!dirWaiters.has(path)) return;
        dirWaiters.delete(path);
        toast(`Could not list ${path}.`, { tone: "error" });
        resolve(null);
      }, SEARCH_TIMEOUT_MS);
    });
  }

  /** Folder mentions waiting on a listing, `path` → resolve. */
  const dirWaiters = new Map();

  /* --- resolving a mention nobody picked ------------------------------------ */

  /* A path only becomes known by being seen in a search answer or a listing, so
   * a mention that was *pasted* or typed out in full — never having opened the
   * menu — was known to nothing and attached nothing. That is the likeliest way
   * to write one once you know the path, so it has to work.
   *
   * Listing the token's parent directory is what settles it: `onList` already
   * remembers every entry with its real `is_dir` and `size`, so one listing
   * turns the guess into the same confirmed entry a pick would have produced. */

  /** Tokens the host says do not exist, so a typo is asked about once and then
   *  left as ordinary text. Cleared whenever the tree may have changed. */
  const missing = new Set();
  /** Listings being awaited, `path` → [resolve]. Shared so two tokens in one
   *  directory cost one request. */
  const listWaiters = new Map();

  function listOnce(path) {
    return new Promise((resolve) => {
      const waiting = listWaiters.get(path);
      if (waiting) {
        waiting.push(resolve);
        return;
      }
      listWaiters.set(path, [resolve]);
      if (!send({ type: "workspace-list", path })) {
        listWaiters.delete(path);
        resolve();
        return;
      }
      setTimeout(() => {
        const pendingList = listWaiters.get(path);
        if (!pendingList) return;
        listWaiters.delete(path);
        pendingList.forEach((r) => r());
      }, SEARCH_TIMEOUT_MS);
    });
  }

  /** Confirms every unknown mention in `text`. Resolves true when it learned
   *  something, so the caller knows the highlight is worth repainting. */
  async function learn(text) {
    const wanted = [];
    TOKEN.lastIndex = 0;
    let match;
    while ((match = TOKEN.exec(text))) {
      const token = trimTail(match[2]);
      // A bare `@word` with no slash is far more often prose ("@here") than a
      // path, but it may name a top-level folder, so the root listing settles
      // it at the cost of one cached request.
      if (!token || lookup(token) || missing.has(token)) continue;
      if (!wanted.includes(token)) wanted.push(token);
    }
    if (!wanted.length) return false;

    let learned = false;
    for (const token of wanted.slice(0, MAX_MENTIONS)) {
      const bare = token.replace(/\/+$/, "");
      const cut = bare.lastIndexOf("/");
      await listOnce(cut < 0 ? "" : bare.slice(0, cut));
      if (lookup(token)) learned = true;
      else missing.add(token);
    }
    return learned;
  }

  // `onList` is also the delivery point for a folder mention's own listing.
  const baseOnList = onList;
  function handleList(frame) {
    baseOnList(frame);
    const path = frame.path ?? "";

    const waitingForPath = listWaiters.get(path);
    if (waitingForPath) {
      listWaiters.delete(path);
      waitingForPath.forEach((r) => r());
    }

    const resolveDir = dirWaiters.get(path);
    if (!resolveDir) return;
    dirWaiters.delete(path);

    const lines = (frame.entries || [])
      .map((e) => (e.is_dir ? `${e.name}/` : `${e.name}  (${e.size} bytes)`))
      .sort();
    const text =
      `Listing of workspace/${path}/ — ${lines.length} entries. ` +
      `File contents are not inlined; read individual files with read_path.\n\n` +
      (lines.join("\n") || "(empty)");
    resolveDir({
      name: `workspace/${path}/ (listing)`,
      mime: "text/plain",
      data: base64(new TextEncoder().encode(text).buffer),
    });
  }

  function key(entry) {
    return entry.is_dir ? `${entry.path}/` : entry.path;
  }

  async function contentsFor(entry) {
    const k = key(entry);
    if (bytes.has(k)) return bytes.get(k);
    const attachment = entry.is_dir ? await fetchDir(entry) : await fetchFile(entry);
    if (attachment) bytes.set(k, attachment);
    return attachment;
  }

  /* --- what the message carries -------------------------------------------- */

  /** Every mention token in `text` naming a path known to exist, with its range
   *  — used for both the highlight and the attachments, so the two can never
   *  disagree about what is attached. */
  function found(text) {
    const out = [];
    TOKEN.lastIndex = 0;
    let match;
    while ((match = TOKEN.exec(text))) {
      const token = trimTail(match[2]);
      if (!token) continue;
      const entry = lookup(token);
      if (!entry) continue;
      const start = match.index + match[1].length;
      out.push({ entry, start, end: start + 1 + token.length });
    }
    return out;
  }

  /** Attachments for a message, in mention order, one per distinct path.
   *
   *  Confirms unknown tokens first, so a pasted path attaches its file just as
   *  a picked one does. This is the last chance to get it right: after this the
   *  message is on the wire. */
  async function attachmentsFor(text) {
    await learn(text);
    const unique = [];
    const seen = new Set();
    for (const hit of found(text)) {
      const k = key(hit.entry);
      if (seen.has(k)) continue;
      seen.add(k);
      unique.push(hit.entry);
    }
    if (unique.length > MAX_MENTIONS) {
      toast(`Only the first ${MAX_MENTIONS} mentioned files are attached.`, { tone: "error" });
      unique.length = MAX_MENTIONS;
    }
    const all = await Promise.all(unique.map(contentsFor));
    return all.filter(Boolean);
  }

  /** True while any mentioned file still has to be read, so the composer knows
   *  a submit will have to wait on the network. */
  function pending(text) {
    return found(text).some((hit) => !bytes.has(key(hit.entry)));
  }

  /* --- highlight ----------------------------------------------------------- */

  /* A mirror div behind the textarea with the same font, padding and width, so
   * its wrapping is identical. Its own glyphs are transparent — the visible
   * text is still the textarea's, which keeps the caret, selection and
   * spellcheck native. Only the highlight rectangles come from the mirror. */
  function paint() {
    if (!mirror) return;
    const text = input.value;
    const nodes = [];
    let at = 0;
    for (const hit of found(text)) {
      if (hit.start > at) nodes.push(document.createTextNode(text.slice(at, hit.start)));
      nodes.push(
        el(
          "span",
          { class: `mention-hit${hit.entry.is_dir ? " is-dir" : ""}` },
          text.slice(hit.start, hit.end)
        )
      );
      at = hit.end;
    }
    // The trailing newline keeps the mirror's height in step with a message
    // ending in one, which otherwise collapses and shifts every highlight.
    nodes.push(document.createTextNode(`${text.slice(at)}\n`));
    clear(mirror).append(...nodes);
    mirror.scrollTop = input.scrollTop;
  }

  /* --- the menu ------------------------------------------------------------ */

  const menu = el("div", { class: "mention-menu", role: "listbox", hidden: true });
  input.parentElement.append(menu);

  const state = {
    open: false,
    query: null,
    from: 0,
    to: 0,
    items: [],
    total: 0,
    active: 0,
    loading: false,
    note: null,
  };

  function close() {
    if (!state.open) return;
    state.open = false;
    state.query = null;
    state.items = [];
    state.note = null;
    inflight = null;
    clearTimeout(debounce);
    clearTimeout(timeout);
    menu.hidden = true;
    clear(menu);
  }

  function drawMenu() {
    clear(menu);
    menu.hidden = false;

    if (state.note) {
      menu.append(el("div", { class: "mention-note" }, state.note));
      return;
    }
    if (state.loading && !state.items.length) {
      menu.append(el("div", { class: "mention-note" }, "Searching the workspace…"));
      return;
    }
    if (!state.items.length) {
      menu.append(
        el(
          "div",
          { class: "mention-note" },
          state.query
            ? `Nothing in the workspace matches “${state.query}”. Keep typing, or Esc to dismiss.`
            : "The workspace is empty — add files in the Files tab."
        )
      );
      return;
    }

    state.items.forEach((entry, i) => {
      menu.append(
        el(
          "button",
          {
            type: "button",
            class: `mention-item${i === state.active ? " is-active" : ""}`,
            role: "option",
            // Must not blur the textarea before the click is handled, or the
            // caret position the insertion needs is already gone.
            onMousedown: (event) => event.preventDefault(),
            onClick: () => choose(entry),
            onMouseenter: () => {
              state.active = i;
              drawMenu();
            },
          },
          el("span", { class: "mention-icon" }, icon(entry.is_dir ? ICONS.dir : ICONS.file, { size: 14 })),
          el("span", { class: "mention-name" }, entry.name + (entry.is_dir ? "/" : "")),
          el("span", { class: "mention-path mono" }, entry.path)
        )
      );
    });

    if (state.total > state.items.length) {
      menu.append(
        el(
          "div",
          { class: "mention-note" },
          `${state.total - state.items.length} more match — keep typing to narrow it.`
        )
      );
    }

    // Keep the highlighted row in view without scrolling the page.
    menu.querySelector(".is-active")?.scrollIntoView({ block: "nearest" });
  }

  /** Replaces the token being typed with the chosen path, and reads it. */
  function choose(entry) {
    remember(entry);
    const token = entry.is_dir ? `${entry.path}/` : entry.path;
    const before = input.value.slice(0, state.from);
    const after = input.value.slice(state.to);
    // A folder keeps its trailing slash so typing straight on narrows into it;
    // a file gets a space, because the next thing typed is prose.
    const insert = `@${token}${entry.is_dir ? "" : " "}`;
    input.value = `${before}${insert}${after}`;
    const caret = state.from + insert.length;
    input.setSelectionRange(caret, caret);
    close();
    onTextChange?.();
    paint();
    input.focus();

    // Read now rather than at send time, so the message is ready to go the
    // moment it is written and a slow read happens while there is still
    // something else to do.
    contentsFor(entry).then(paint);

    // A folder pick is usually a step towards a file inside it: reopen the menu
    // on the deeper prefix rather than making the user type `@` again.
    if (entry.is_dir) refresh();
  }

  /** Looks at the caret and opens, updates or closes the menu. */
  function refresh() {
    const caret = input.selectionStart ?? 0;
    const match = AT_CARET.exec(input.value.slice(0, caret));
    if (!match) {
      close();
      return;
    }

    const query = match[1];
    const unchanged = state.open && state.query === query;
    state.query = query;
    state.from = caret - query.length - 1;
    state.to = caret;
    state.open = true;
    state.note = null;
    if (!unchanged) {
      state.loading = true;
      search(query);
    }
    drawMenu();
  }

  /** Keys the menu owns while it is open. Returns true when it consumed one, so
   *  the composer knows not to send. */
  function handleKey(event) {
    if (!state.open) return false;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        if (!state.items.length) return false;
        const step = event.key === "ArrowDown" ? 1 : -1;
        state.active = (state.active + step + state.items.length) % state.items.length;
        drawMenu();
        return true;
      }
      case "Enter":
      case "Tab":
        if (!state.items.length) return false;
        choose(state.items[state.active]);
        return true;
      case "Escape":
        close();
        return true;
      default:
        return false;
    }
  }

  /* Confirming as the text settles is what makes a pasted mention light up
   * before it is sent, rather than only being attached silently at submit. It
   * is debounced because it costs a listing, and skipped while the menu is open
   * since a search is already answering the same question. */
  let learning = null;
  function learnSoon() {
    clearTimeout(learning);
    learning = setTimeout(async () => {
      if (state.open) return;
      if (await learn(input.value)) paint();
    }, 220);
  }

  input.addEventListener("input", () => {
    refresh();
    paint();
    learnSoon();
  });
  input.addEventListener("paste", learnSoon);
  input.addEventListener("click", refresh);
  input.addEventListener("scroll", () => {
    if (mirror) mirror.scrollTop = input.scrollTop;
  });
  input.addEventListener("blur", () => {
    // Deferred: a click on a menu row blurs the textarea first, and closing
    // here would remove the row before its own handler ran.
    setTimeout(close, 120);
  });

  return {
    onFind,
    onList: handleList,
    onResult,
    invalidate,
    handleKey,
    paint,
    attachmentsFor,
    pending,
    close,
    refresh,
  };
}
