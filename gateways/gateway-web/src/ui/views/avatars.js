/* Avatars: a face per turn, in the gutter beside the conversation.
 *
 * Each turn gets a square tile in the transcript's left gutter — yours on your
 * rows, the agent's on its own — so a long transcript can be skimmed by picture
 * rather than by reading each label. The tiles are cloned from two <template>
 * elements in index.html, which is what keeps the markup for a face in one
 * place instead of spread across the renderers that build rows.
 *
 * Placing them is CSS's job, not this module's: a tile is appended to its row
 * and lifted out of flow into the gutter by .turn-avatar. Nothing here measures
 * or positions anything.
 *
 * The two avatars come from different places on purpose:
 *
 *   the agent's  is identity. `agent.avatar` in config, substituted into the
 *                template at serve time, the same value as the favicon and the
 *                sidebar brand. Not editable here — an installation's agent is
 *                not something a chat window renames.
 *   the user's   is a preference. Held in the host KV store, because config is
 *                read only at startup and a picture chosen now must appear now.
 *
 * That difference shows up in the code as the one interesting problem here: the
 * agent's picture is known before the first paint, and yours arrives over the
 * wire afterwards, by which point rows are already on screen. So `draw` updates
 * every tile already in the DOM as well as recording the value for the tiles
 * still to come. A replayed transcript of two hundred turns then picks up a new
 * upload without being re-rendered.
 *
 * Both follow the brand's fallback pattern: an <img> and a drawn mark are both
 * in the markup and exactly one is shown, so a URL that 404s degrades to the
 * mark rather than to a broken-image glyph.
 */

import { $, setHidden } from "../lib/dom.js";
import { toast } from "../lib/toast.js";

/* The longest edge an uploaded picture is stored at.
 *
 * The image is re-encoded in the browser before it goes anywhere. Two reasons
 * beyond politeness to the store: a phone photo is several megabytes of base64
 * on every page load of every tab, and a tile is displayed at 44 CSS pixels, so
 * anything past this is bytes nobody can see. 256 leaves headroom for a 3x
 * display and for anywhere the picture might later be shown larger. */
const MAX_EDGE = 256;
/* WebP at this quality is visually clean at tile size and lands well inside the
 * host's own ceiling. Alpha is preserved, which a JPEG would flatten. */
const FORMAT = "image/webp";
const QUALITY = 0.86;
/* Refused before decoding. The host caps the stored string too; this one exists
 * so a 40 MB drag never gets read into memory at all. */
const MAX_UPLOAD_BYTES = 12 * 1024 * 1024;

let send = null;
/* The user's picture as last known: "" for none. Read by every tile minted
 * after a frame lands, which is what makes a late upload apply to rows that
 * were rendered before it. */
let userAvatar = "";

/**
 * Wires the sidebar button and the agent template's failure path.
 *
 * @param {(frame: object) => void} sendFrame  the connection's send
 */
export function mountAvatars(sendFrame) {
  send = sendFrame;

  const input = $("user-avatar-input");
  const pick = () => input?.click();
  $("user-avatar-btn")?.addEventListener("click", pick);

  input?.addEventListener("change", () => {
    const file = input.files?.[0];
    // Reset first: choosing the same file twice in a row fires no `change`
    // event otherwise, which looks exactly like a control that has broken.
    input.value = "";
    if (file) upload(file);
  });

  wireFallback($("user-avatar-img"), $("user-avatar-mark"));
}

/**
 * An avatar tile for a turn, or null for a role that has no face.
 *
 * Cloned rather than built, so the agent's serve-time `{agent_avatar}` and its
 * hidden-or-not decision are made once in the markup and every row inherits
 * them.
 *
 * @param {string} role  the event's role: "user" and "assistant" have faces
 * @returns {Node|null}
 */
export function turnAvatar(role) {
  const id = role === "user" ? "user-avatar-template" : role === "assistant" ? "agent-avatar-template" : null;
  if (!id) return null;
  const template = $(id);
  if (!template) return null;

  const node = template.content.firstElementChild?.cloneNode(true);
  if (!node) return null;

  const img = node.querySelector(".turn-img");
  const mark = node.querySelector(".turn-mark");
  // The user's tile is filled in here rather than in the markup: unlike the
  // agent's, its source is not known when the page is served.
  if (role === "user") paint(img, mark, userAvatar);
  wireFallback(img, mark);
  return node;
}

/** The host's answer to `user-avatar` — and to any tab's `user-avatar-set`. */
export function onFrame(frame) {
  draw(frame.avatar || "");
}

/** Asks for the stored picture. Sent on `hello`, and again on a reconnect. */
export function request() {
  send?.({ type: "user-avatar" });
}

// --- drawing ------------------------------------------------------------------

/* Shows a picture everywhere the user appears: the sidebar button, and every
 * turn tile already rendered. One function so no two places on screen can
 * disagree about who you are. */
function draw(url) {
  userAvatar = url || "";
  paint($("user-avatar-img"), $("user-avatar-mark"), userAvatar);
  for (const tile of document.querySelectorAll(".turn-avatar.is-user")) {
    paint(tile.querySelector(".turn-img"), tile.querySelector(".turn-mark"), userAvatar);
  }
}

/* Shows exactly one of the image and the drawn mark. */
function paint(img, mark, url) {
  if (!img || !mark) return;
  // setHidden rather than `.hidden =`: the mark is an <svg>, and SVGElement does
  // not inherit the `hidden` IDL attribute, so the assignment form sets a dead
  // JS property and leaves the mark showing behind the image. That is exactly
  // what the first browser run of this found.
  if (url) {
    img.src = url;
    setHidden(img, false);
    setHidden(mark, true);
  } else {
    // The attribute is removed rather than set empty: an empty `src` resolves to
    // the page itself and the browser reports it as a failed image.
    img.removeAttribute("src");
    setHidden(img, true);
    setHidden(mark, false);
  }
}

/* The brand's rule, applied to every avatar: a configured URL is arbitrary and
 * can 404, be blocked, or not be an image. Swap to the mark rather than leaving
 * a broken-image glyph where a face should be. */
function wireFallback(img, mark) {
  if (!img || !mark) return;
  img.addEventListener("error", () => {
    setHidden(img, true);
    setHidden(mark, false);
  });
}

// --- uploading ----------------------------------------------------------------

async function upload(file) {
  if (!file.type.startsWith("image/")) {
    toast(`${file.name} is not an image.`, { tone: "error" });
    return;
  }
  if (file.size > MAX_UPLOAD_BYTES) {
    toast(`${file.name} is too large — pick one under 12 MB.`, { tone: "error" });
    return;
  }

  let data;
  try {
    data = await downscale(file);
  } catch (err) {
    // A file the decoder rejects: a renamed non-image, a truncated download, a
    // format this browser does not carry. Say which file, since the picker may
    // be long closed by now.
    toast(`Could not read ${file.name} as an image.`, { tone: "error" });
    console.warn("avatar decode failed", err);
    return;
  }

  // Held so Undo has somewhere to go back to; read before the optimistic draw
  // overwrites it. "" when there was no picture, which is what lets the same
  // action *remove* a first upload as well as revert a replacement.
  const restore = userAvatar;
  // Drawn at once. The host echoes the stored value back and `draw` runs again,
  // so a refusal corrects this rather than being papered over by it.
  draw(data);
  send?.({ type: "user-avatar-set", avatar: data });

  toast("Avatar updated.", {
    tone: "good",
    action: {
      label: restore ? "Undo" : "Remove",
      run: () => {
        draw(restore);
        send?.({ type: "user-avatar-set", avatar: restore });
      },
    },
  });
}

/* Decodes, crops to the tile's square, and re-encodes as WebP.
 *
 * Cropping here rather than leaving it to `object-fit` means the stored bytes
 * are only the pixels that get displayed, and that every place the picture is
 * shown shows the same part of it — the top, where a face is.
 */
async function downscale(file) {
  const bitmap = await createImageBitmap(file);
  try {
    // The largest square that fits inside the source, anchored at the top.
    const edge = Math.min(bitmap.width, bitmap.height);
    const cropX = (bitmap.width - edge) / 2;

    const scale = Math.min(1, MAX_EDGE / edge);
    const out = Math.max(1, Math.round(edge * scale));

    const canvas = document.createElement("canvas");
    canvas.width = out;
    canvas.height = out;
    const ctx = canvas.getContext("2d");
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(bitmap, cropX, 0, edge, edge, 0, 0, out, out);

    const blob = await new Promise((resolve, reject) =>
      canvas.toBlob((b) => (b ? resolve(b) : reject(new Error("encode failed"))), FORMAT, QUALITY)
    );
    return await asDataUrl(blob);
  } finally {
    bitmap.close?.();
  }
}

function asDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error || new Error("read failed"));
    reader.readAsDataURL(blob);
  });
}
