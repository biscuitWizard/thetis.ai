/* The shared workspace, as a file explorer.
 *
 * One directory at a time, rendered into the same slide-over host every
 * inspector uses. The host answers `workspace-*` frames from the gateway
 * process itself (the UI guest has no filesystem), and every mutation comes
 * back with a fresh listing of the directory it touched, so this view never
 * guesses at the outcome of anything — it draws what the host said is there.
 *
 * Raw bytes go over HTTP (`/workspace/file/…`): an image previews from an
 * <img>, a download is a link, an upload is a PUT of the browser's own File.
 *
 * Dropping onto the listing uploads into the folder on screen; dropping onto a
 * folder row or a breadcrumb uploads into *that* folder instead. A dropped
 * directory is walked with the entries API and uploaded whole, each file
 * carrying its relative path, so the tree is rebuilt server-side.
 */

import { el, icon } from "../lib/dom.js";
import { popover, toast } from "../lib/toast.js";
import * as rail from "./rail.js";

let send = () => {};

/** What the explorer is looking at. `path` is the directory shown ("" = the
 *  workspace root); `file` is an open preview, drawn instead of the listing. */
const state = {
  path: "",
  listing: null,
  file: null,
  editing: false,
  draft: "",
  status: null, // { ok, message } from the last operation
  /** True while an upload is running. A `workspace-result` arriving mid-upload
   *  — from a mkdir this upload itself fired, or from the agent working in the
   *  background — must not overwrite the progress line with something older. */
  uploading: false,
};

const ICONS = {
  dir: ["M2.5 5.5a1 1 0 0 1 1-1h4l1.6 1.8h7.4a1 1 0 0 1 1 1v8.2a1 1 0 0 1-1 1h-13a1 1 0 0 1-1-1z"],
  image: ["M3 4.5h14v11H3z", "M6 12.5l3-3 2.5 2.5 2-2 3.5 3.5"],
  audio: ["M7 13.5V6l8-1.5V12", "M7 13.5a1.8 1.8 0 1 1-3.6 0 1.8 1.8 0 0 1 3.6 0", "M15 12a1.8 1.8 0 1 1-3.6 0 1.8 1.8 0 0 1 3.6 0"],
  video: ["M3 5h10.5v10H3z", "M13.5 8.5 17 6.5v7l-3.5-2"],
  archive: ["M3.5 6h13v9.5h-13z", "M3.5 6l1-2h11l1 2", "M8.5 9h3"],
  code: ["M7 6.5 3.5 10 7 13.5", "M13 6.5l3.5 3.5-3.5 3.5"],
  file: ["M5 3.5h7l3 3v10H5z", "M12 3.5v3h3"],
};

function fileIcon(kind) {
  return icon(ICONS[kind] || ICONS.file, { size: 15, width: 1.6 });
}

function human(n) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = Number(n) || 0;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return unit === 0 ? `${value} B` : `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

function when(ms) {
  return ms ? new Date(ms).toLocaleString() : "";
}

/** The raw-bytes URL for a workspace path, encoded a segment at a time. */
function rawUrl(rel) {
  const segments = rel.split("/").filter(Boolean).map(encodeURIComponent);
  return `/workspace/file/${segments.join("/")}`;
}

function joinPath(dir, name) {
  return dir ? `${dir}/${name}` : name;
}

// --- talking to the host ------------------------------------------------------

function list(path) {
  state.path = path;
  send({ type: "workspace-list", path });
}

function openFile(path) {
  send({ type: "workspace-read", path });
}

/** PUT each item into `destDir`, then relist. Sequential on purpose: a failure
 *  names the file that failed and stops there.
 *
 *  An item is `{file, path}` where `path` is relative to `destDir` — a plain
 *  pick has `path === file.name`, a dropped folder carries the subpath, and the
 *  host's PUT handler creates the parent folders it needs. `dirs` lists every
 *  folder in a dropped tree so the empty ones still get created.
 *
 *  `destDir` may be a folder the view is not currently showing (a drop onto a
 *  folder row), so the relist at the end is always of `state.path`. */
async function upload(items, destDir = state.path, dirs = []) {
  if (!items.length && !dirs.length) return;

  state.uploading = true;
  try {
    await uploadRun(items, destDir, dirs);
  } finally {
    state.uploading = false;
  }
}

async function uploadRun(items, destDir, dirs) {
  const total = items.length;
  const where = destDir || "workspace";
  let bytes = 0;
  let last = 0;

  // Folders first, and only the ones no file will create on its way in: the
  // host's PUT makes parents as needed, so a folder with anything beneath it
  // needs no mkdir, but an empty one would otherwise vanish from the upload.
  // Doing this before the files means the last word on the status line is the
  // upload summary rather than a `workspace-result` for the final mkdir.
  const filled = new Set(items.map(({ path }) => path.split("/").slice(0, -1).join("/")));
  for (const dir of dirs) {
    let covered = false;
    for (const parent of filled) {
      if (parent === dir || parent.startsWith(`${dir}/`)) {
        covered = true;
        break;
      }
    }
    if (!covered) send({ type: "workspace-mkdir", path: joinPath(destDir, dir) });
  }

  for (let i = 0; i < total; i++) {
    const { file, path } = items[i];
    // Redrawing per file would cost more than the upload on a large tree, so
    // the progress line refreshes at most every 120ms (plus the first).
    const now = Date.now();
    if (i === 0 || now - last > 120) {
      last = now;
      state.status = { ok: true, message: `uploading ${path} → ${where} (${i + 1} of ${total})…` };
      draw();
    }
    try {
      const res = await fetch(rawUrl(joinPath(destDir, path)), { method: "PUT", body: file });
      if (!res.ok) throw new Error((await res.text()) || res.statusText);
      bytes += file.size;
    } catch (e) {
      state.status = { ok: false, message: `upload of ${path} failed: ${e.message}` };
      toast(`Upload of ${path} failed: ${e.message}`, { tone: "error" });
      list(state.path);
      draw();
      return;
    }
  }

  if (!total) {
    // Folders only: the mkdir replies carry their own status, and relisting
    // shows them.
    list(state.path);
    return;
  }

  const what = total === 1 ? items[0].path : `${total} files`;
  state.status = { ok: true, message: `uploaded ${what} into ${where} (${human(bytes)})` };
  toast(`Uploaded ${what} into ${where}.`, { tone: "good" });
  list(state.path);
}

// --- drag and drop ------------------------------------------------------------

/** Files accepted from one drop. A runaway folder (or a symlink loop inside
 *  one) would otherwise queue an unbounded number of PUTs. */
const MAX_DROP_FILES = 2000;
const MAX_DROP_DEPTH = 24;

/** True when this drag carries files rather than text or a UI element. */
function carriesFiles(event) {
  const types = event.dataTransfer?.types;
  return !!types && [...types].includes("Files");
}

/** The dropped items, read *synchronously*: `dataTransfer` is neutered as soon
 *  as the drop handler yields, so the entries must be taken before any await.
 *  Returns FileSystemEntry objects where the browser offers them (that is what
 *  makes a folder drop possible) and falls back to plain Files. */
function dropItems(dataTransfer) {
  const out = [];
  // Indexed, not iterated: DataTransferItemList is array-like but is not
  // reliably iterable, and a for...of over it throws in some browsers.
  const items = dataTransfer.items;
  for (let i = 0; i < (items?.length || 0); i++) {
    const item = items[i];
    if (item.kind !== "file") continue;
    const entry = item.webkitGetAsEntry?.();
    if (entry) out.push(entry);
    else {
      const file = item.getAsFile();
      if (file) out.push(file);
    }
  }
  if (out.length) return out;

  // No items list at all: only flat files are available, so a dropped folder
  // cannot be seen. Better to upload the files than to refuse the drop.
  const files = dataTransfer.files;
  for (let i = 0; i < (files?.length || 0); i++) out.push(files[i]);
  return out;
}

/** `readEntries` hands back a bounded batch (~100), so it is called until dry. */
function readAll(reader) {
  return new Promise((resolve, reject) => {
    const all = [];
    const step = () =>
      reader.readEntries((batch) => {
        if (!batch.length) resolve(all);
        else {
          all.push(...batch);
          step();
        }
      }, reject);
    step();
  });
}

function entryFile(entry) {
  return new Promise((resolve, reject) => entry.file(resolve, reject));
}

/** Depth-first walk of one dropped entry, accumulating files and folders with
 *  paths relative to the drop. */
async function walkEntry(entry, prefix, files, dirs, depth) {
  const path = prefix ? `${prefix}/${entry.name}` : entry.name;
  if (entry.isFile) {
    files.push({ file: await entryFile(entry), path });
    return;
  }
  if (!entry.isDirectory || depth >= MAX_DROP_DEPTH) return;
  dirs.push(path);
  for (const child of await readAll(entry.createReader())) {
    if (files.length >= MAX_DROP_FILES) return;
    await walkEntry(child, path, files, dirs, depth + 1);
  }
}

/** Expands the synchronously-captured items into flat upload work. */
async function expandDrop(items) {
  const files = [];
  const dirs = [];
  for (const item of items) {
    if (files.length >= MAX_DROP_FILES) break;
    if (item instanceof File) files.push({ file: item, path: item.name });
    else await walkEntry(item, "", files, dirs, 0);
  }
  return { files, dirs, truncated: files.length >= MAX_DROP_FILES };
}

/* Exactly one zone is highlighted at a time, tracked here rather than per node.
 *
 * Counting dragenter/dragleave pairs per node does not survive nesting: a
 * folder row sits inside the listing zone, both want the highlight, and the
 * leave events that would balance the books go missing when a drag ends outside
 * the window. Instead each zone stops the event at the innermost handler, so
 * the deepest zone under the cursor is the only one that hears `dragover` — it
 * claims the highlight, and a lapsing timer gives it back. A timer cannot get
 * stuck the way a counter can: when events stop arriving, the paint clears.
 *
 * The highlight is a class toggled in place, never a redraw — redrawing on
 * dragover would replace the node under the cursor and cancel the drag. */
const HOVER_LINGER_MS = 220;
let hotZone = null;
let hotTimer = null;

function clearHot() {
  clearTimeout(hotTimer);
  hotTimer = null;
  hotZone?.classList.remove("is-drop");
  hotZone = null;
}

function markHot(node) {
  if (hotZone !== node) {
    hotZone?.classList.remove("is-drop");
    hotZone = node;
    node.classList.add("is-drop");
  }
  clearTimeout(hotTimer);
  hotTimer = setTimeout(clearHot, HOVER_LINGER_MS);
}

window.addEventListener("dragend", clearHot);
window.addEventListener("blur", clearHot);

/** Makes `node` accept a file drop, uploading into `destPath`. `label` is how
 *  the destination is named back to the user ("workspace" for the root). */
function dropTarget(node, destPath, label) {
  if (!node) return node;

  // dragenter is claimed too, so the highlight appears on the first event
  // rather than waiting for the first dragover, and so it is stopped from
  // reaching the zone behind this one.
  for (const kind of ["dragenter", "dragover"]) {
    node.addEventListener(kind, (event) => {
      if (!carriesFiles(event)) return;
      event.preventDefault();
      event.stopPropagation();
      event.dataTransfer.dropEffect = "copy";
      markHot(node);
    });
  }
  node.addEventListener("drop", (event) => {
    if (!carriesFiles(event)) return;
    event.preventDefault();
    // Keeps the drop from also reaching the composer's window-level handler,
    // which would attach the same files to the message being typed.
    event.stopPropagation();
    clearHot();
    handleDrop(event.dataTransfer, destPath, label);
  });

  node.dataset.dropInto = label;
  return node;
}

async function handleDrop(dataTransfer, destPath, label) {
  const items = dropItems(dataTransfer);
  if (!items.length) {
    toast("Nothing to upload — that drop carried no files.", { tone: "error" });
    return;
  }

  state.status = { ok: true, message: `reading ${items.length} dropped item${items.length === 1 ? "" : "s"}…` };
  draw();

  let work;
  try {
    work = await expandDrop(items);
  } catch (e) {
    state.status = { ok: false, message: `could not read the dropped items: ${e.message}` };
    toast(`Could not read the dropped items: ${e.message}`, { tone: "error" });
    draw();
    return;
  }

  if (work.truncated) {
    toast(`That drop holds more than ${MAX_DROP_FILES} files; uploading the first ${MAX_DROP_FILES} into ${label}.`, {
      tone: "error",
    });
  }
  if (!work.files.length && !work.dirs.length) {
    state.status = { ok: false, message: "nothing in that drop to upload" };
    toast("Nothing in that drop to upload.", { tone: "error" });
    draw();
    return;
  }

  await upload(work.files, destPath, work.dirs);
}

// --- frame handlers (wired from app.js) ---------------------------------------

export function onList(frame) {
  // Mutations relist the directory they touched; adopt it only when it is the
  // one on screen, so a write into a subfolder cannot yank the view around.
  if (frame.path !== state.path) return;
  state.listing = frame;
  if (!state.file && rail.isOpen("workspace")) draw();
}

export function onFile(frame) {
  state.file = frame;
  state.editing = false;
  state.draft = frame.text || "";
  if (rail.isOpen("workspace")) draw();
}

export function onResult(frame) {
  // A failure always speaks, even mid-upload — but a success must not replace
  // the progress line, or the mkdirs an upload fires would report "created
  // folder X" over the top of "uploading 40 of 300".
  if (!state.uploading || !frame.ok) state.status = { ok: frame.ok, message: frame.message };
  if (!rail.isOpen("workspace")) return;
  // The host relists the directory the mutation touched, but a move across
  // directories touches one this view is not showing — and a failure relists
  // nothing. Asking again for the directory on screen keeps the view honest
  // in both cases, at the cost of one duplicate listing in the common one.
  if (!state.file) list(state.path);
  draw();
}

// --- drawing -------------------------------------------------------------------

function crumbs() {
  const parts = state.path.split("/").filter(Boolean);
  const nodes = [
    dropTarget(
      el(
        "button",
        { type: "button", class: "ws-crumb", title: "The workspace root — drop here to upload into it",
          onClick: () => { state.file = null; list(""); } },
        "workspace"
      ),
      "",
      "workspace"
    ),
  ];
  let acc = "";
  for (const part of parts) {
    acc = joinPath(acc, part);
    const target = acc;
    nodes.push(el("span", { class: "ws-crumb-sep" }, "/"));
    nodes.push(
      dropTarget(
        el(
          "button",
          { type: "button", class: "ws-crumb", title: `Drop here to upload into ${target}`,
            onClick: () => { state.file = null; list(target); } },
          part
        ),
        target,
        target
      )
    );
  }
  return el("nav", { class: "ws-crumbs", "aria-label": "Workspace path" }, nodes);
}

function toolbar() {
  const input = el("input", { type: "file", multiple: true, hidden: true });
  input.addEventListener("change", () => {
    // `upload` takes {file, path} so a dropped tree can carry subpaths; a
    // picked file is just its own name at the top of the destination.
    if (input.files.length) upload([...input.files].map((file) => ({ file, path: file.name })));
    input.value = "";
  });

  return el(
    "div",
    { class: "ws-toolbar" },
    crumbs(),
    el(
      "div",
      { class: "ws-actions" },
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn",
          title: "Create an empty file here and open it for editing",
          onClick: (event) => {
            popover(event.currentTarget, {
              message: "New file",
              input: { placeholder: "notes.md", value: "notes.md", mono: true },
              confirmLabel: "Create",
              onConfirm: (name) => {
                const path = joinPath(state.path, name);
                send({ type: "workspace-write", path, text: "" });
                openFile(path);
              },
            });
          },
        },
        "New file"
      ),
      el(
        "button",
        {
          type: "button",
          class: "ghost-btn",
          title: "Create a folder here",
          onClick: (event) => {
            popover(event.currentTarget, {
              message: "New folder",
              input: { placeholder: "name", mono: true },
              confirmLabel: "Create",
              onConfirm: (name) => send({ type: "workspace-mkdir", path: joinPath(state.path, name) }),
            });
          },
        },
        "New folder"
      ),
      el(
        "button",
        { type: "button", class: "ghost-btn", title: "Upload files into this folder", onClick: () => input.click() },
        "Upload"
      ),
      // The listing follows the agent's tool results on its own; this is for
      // anything that changed the directory without a tool running.
      el(
        "button",
        { type: "button", class: "ghost-btn", title: "List this folder again", onClick: () => list(state.path) },
        "Refresh"
      ),
      input
    )
  );
}

function entryRow(entry) {
  const enter = () => (entry.is_dir ? list(entry.path) : openFile(entry.path));

  const meta = entry.is_dir
    ? when(entry.modified_ms)
    : [human(entry.size), when(entry.modified_ms)].filter(Boolean).join(" · ");

  const actions = el(
    "div",
    { class: "ws-row-actions" },
    !entry.is_dir &&
      el(
        "a",
        { class: "ghost-btn", href: `${rawUrl(entry.path)}?download=1`, title: "Download this file", download: entry.name },
        "Download"
      ),
    el(
      "button",
      {
        type: "button",
        class: "ghost-btn",
        title: "Rename or move (edit the whole path to move)",
        onClick: (event) => {
          event.stopPropagation();
          popover(event.currentTarget, {
            message: `Rename or move ${entry.name}`,
            detail: "Edit the whole path to move it between folders.",
            input: { value: entry.path, mono: true },
            confirmLabel: "Rename",
            onConfirm: (to) => {
              if (to === entry.path) return;
              send({ type: "workspace-move", from: entry.path, to });
            },
          });
        },
      },
      "Rename"
    ),
    el(
      "button",
      {
        type: "button",
        class: "ghost-btn is-danger",
        title: entry.is_dir ? "Delete this folder and everything in it" : "Delete this file",
        onClick: (event) => {
          event.stopPropagation();
          popover(event.currentTarget, {
            message: entry.is_dir
              ? `Delete the folder ${entry.path}?`
              : `Delete ${entry.path}?`,
            detail: entry.is_dir ? "Everything in it goes too. This cannot be undone." : "This cannot be undone.",
            confirmLabel: "Delete",
            danger: true,
            onConfirm: () => send({ type: "workspace-delete", path: entry.path, recursive: entry.is_dir }),
          });
        },
      },
      "Delete"
    )
  );

  const row = el(
    "div",
    { class: `ws-row${entry.is_dir ? " is-dir" : ""}`, role: "button", tabindex: "0", onClick: enter,
      title: entry.is_dir ? `Drop files here to upload into ${entry.path}` : undefined,
      onKeydown: (event) => { if (event.key === "Enter") enter(); } },
    el("span", { class: `ws-icon is-${entry.kind}` }, fileIcon(entry.kind)),
    el(
      "span",
      { class: "ws-name" },
      entry.name,
      entry.is_dir ? "/" : "",
      entry.link ? el("span", { class: "pill", title: "A symbolic link; its target may sit outside the workspace" }, "link") : null
    ),
    el("span", { class: "ws-meta" }, meta),
    actions
  );

  // Only a folder is its own destination; a drop on a file row falls through to
  // the listing behind it and lands in the folder on screen.
  return entry.is_dir ? dropTarget(row, entry.path, entry.path) : row;
}

function statusLine() {
  if (!state.status) return null;
  return el("p", { class: `ws-status${state.status.ok ? "" : " is-error"}` }, state.status.message);
}

function listingBlocks() {
  const listing = state.listing;
  const blocks = [toolbar(), statusLine()];

  if (!listing) {
    blocks.push(el("div", { class: "panel-note" }, "Loading…"));
    return blocks;
  }

  // The whole listing is a drop zone for the folder on screen; folder rows
  // inside it are their own zones and stop the event before it gets here.
  const where = state.path || "workspace";
  const zone = dropTarget(
    el(
      "div",
      { class: "ws-drop" },
      listing.entries.length
        ? el("div", { class: "ws-list" }, listing.entries.map(entryRow))
        : el("div", { class: "panel-note" }, "This folder is empty — drop files or a folder here, or use Upload."),
      el("p", { class: "ws-drop-hint" }, `Drop to upload into ${where}`)
    ),
    state.path,
    where
  );
  blocks.push(zone);

  if (listing.truncated) {
    blocks.push(el("p", { class: "ws-status is-error" }, "This folder holds more entries than can be listed; showing the first 5,000."));
  }
  return blocks;
}

function fileBlocks() {
  const file = state.file;
  const back = el(
    "button",
    { type: "button", class: "ghost-btn", onClick: () => { state.file = null; state.editing = false; list(state.path); } },
    "← Back to folder"
  );

  const head = el(
    "div",
    { class: "ws-file-head" },
    back,
    el("span", { class: "ws-file-name mono" }, file.path || file.name),
    el("span", { class: "ws-meta" }, [human(file.size), when(file.modified_ms)].filter(Boolean).join(" · ")),
    el("a", { class: "ghost-btn", href: `${file.url}?download=1`, download: file.name }, "Download")
  );

  const blocks = [head, statusLine()];

  if (state.editing) {
    const editor = el("textarea", { class: "ws-editor", spellcheck: "false" });
    editor.value = state.draft;
    editor.addEventListener("input", () => (state.draft = editor.value));
    blocks.push(editor);
    blocks.push(
      el(
        "div",
        { class: "ws-file-actions" },
        el(
          "button",
          {
            type: "button",
            class: "ghost-btn is-primary",
            onClick: () => {
              send({ type: "workspace-write", path: file.path, text: state.draft });
              // Re-read rather than patching locally: the reply carries the
              // new size and mtime, which the header would otherwise misstate.
              openFile(file.path);
            },
          },
          "Save"
        ),
        el(
          "button",
          {
            type: "button",
            class: "ghost-btn",
            onClick: () => {
              state.editing = false;
              state.draft = file.text || "";
              draw();
            },
          },
          "Cancel"
        )
      )
    );
    setTimeout(() => editor.focus(), 0);
    return blocks;
  }

  if (file.kind === "image") {
    blocks.push(el("img", { class: "ws-preview", src: file.url, alt: file.name }));
  } else if (file.kind === "video") {
    blocks.push(el("video", { class: "ws-preview", src: file.url, controls: true }));
  } else if (file.kind === "audio") {
    blocks.push(el("audio", { class: "ws-audio", src: file.url, controls: true }));
  } else if (file.kind === "pdf") {
    blocks.push(
      el("p", { class: "panel-note" },
        el("a", { href: file.url, target: "_blank", rel: "noopener" }, `Open ${file.name} in a new tab`))
    );
  } else if (file.text_available) {
    blocks.push(el("pre", { class: "ws-text" }, file.text ?? ""));
    blocks.push(
      el(
        "div",
        { class: "ws-file-actions" },
        el(
          "button",
          {
            type: "button",
            class: "ghost-btn",
            onClick: () => {
              state.editing = true;
              state.draft = file.text || "";
              draw();
            },
          },
          "Edit"
        )
      )
    );
  } else {
    blocks.push(
      el("p", { class: "panel-note" },
        "This file is binary (or too large to show inline). Download it to look inside.")
    );
  }
  return blocks;
}

function draw() {
  const listing = state.listing;
  const subtitle = state.file
    ? state.file.mime
    : listing
      ? `${listing.files} file${listing.files === 1 ? "" : "s"} · ${human(listing.bytes)}${listing.path ? ` · in ${listing.path}` : ""}`
      : undefined;

  rail.open({
    id: "workspace",
    title: "Files",
    subtitle,
    blocks: state.file ? fileBlocks() : listingBlocks(),
  });
}

/** Called once from app.js. `sendFn` puts a frame on the socket. */
export function mountWorkspace(sendFn) {
  send = sendFn;
  return {
    open() {
      state.file = null;
      state.editing = false;
      state.status = null;
      draw();
      list(state.path);
    },

    /** The agent touched the filesystem; redraw what is on screen.
     *
     * Nothing here guesses at what changed — it asks the host again, the same
     * way the view's own mutations do. An open editor is the exception: the
     * draft in it is the one thing on this surface the host cannot give back,
     * so a refresh leaves it alone. */
    refresh() {
      if (state.editing) return;
      if (state.file) openFile(state.file.path);
      else list(state.path);
    },
  };
}
