/* Avatars: who is talking, shown either side of the conversation.
 *
 * Two portraits flank the transcript — the user on the left, the agent on the
 * right, matching the side each one's messages are attributed on. They live in
 * the slack a wide window already had between the sidebars and the text, and
 * CSS retracts them when that slack runs out, so this module never measures
 * anything: it only decides *what* each frame shows.
 *
 * The two avatars come from different places on purpose:
 *
 *   the agent's  is identity. `agent.avatar` in config, substituted into the
 *                markup at serve time, the same value as the favicon and the
 *                sidebar brand. Not editable here — an installation's agent is
 *                not something a chat window renames.
 *   the user's   is a preference. Held in the host KV store, because config is
 *                read only at startup and a picture chosen now must appear now.
 *
 * Both follow the brand's fallback pattern: an <img> and a drawn mark are both
 * in the markup and exactly one is shown, so a URL that 404s degrades to the
 * mark rather than to a broken-image glyph.
 */

import { $, AGENT_NAME, setHidden } from "../lib/dom.js";
import { toast } from "../lib/toast.js";

/* The longest edge an uploaded picture is stored at.
 *
 * The image is re-encoded in the browser before it goes anywhere. Two reasons
 * beyond politeness to the store: a phone photo is several megabytes of base64
 * on every page load of every tab, and the portrait is displayed at ~200 CSS
 * pixels, so anything past this is bytes nobody can see. 640 leaves headroom
 * for a 2x display and for the sidebar's 22px copy. */
const MAX_EDGE = 640;
/* WebP at this quality is visually clean on a portrait and lands well inside
 * the host's own ceiling. Alpha is preserved, which a JPEG would flatten. */
const FORMAT = "image/webp";
const QUALITY = 0.86;
/* Refused before decoding. The host caps the stored string too; this one exists
 * so a 40 MB drag never gets read into memory at all. */
const MAX_UPLOAD_BYTES = 12 * 1024 * 1024;

let send = null;

/**
 * Wires both portraits and the sidebar button.
 *
 * @param {(frame: object) => void} sendFrame  the connection's send
 */
export function mountAvatars(sendFrame) {
  send = sendFrame;

  // The agent's, from the markup. Only the failure path needs wiring: which of
  // the two elements starts visible was decided at serve time.
  fallbackOnError($("agent-portrait-img"), $("agent-portrait-mark"));

  const name = $("stage-agent")?.querySelector(".portrait-name");
  if (name) name.textContent = AGENT_NAME;

  // The user's, in two places at once: the sidebar's 22px button and the
  // portrait beside the chat. Either opens the file picker.
  const input = $("user-avatar-input");
  const pick = () => input?.click();
  $("user-avatar-btn")?.addEventListener("click", pick);
  $("stage-user-portrait")?.addEventListener("click", pick);

  input?.addEventListener("change", () => {
    const file = input.files?.[0];
    // Reset first: choosing the same file twice in a row fires no `change`
    // event otherwise, which looks exactly like a control that has broken.
    input.value = "";
    if (file) upload(file);
  });

  fallbackOnError($("user-avatar-img"), $("user-avatar-mark"));
  fallbackOnError($("user-portrait-img"), $("user-portrait-mark"));
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

/* Shows a picture in both places it appears, or the drawn mark in both when
 * there is none. One function so the sidebar and the portrait can never
 * disagree about who you are. */
function draw(url) {
  for (const [imgId, markId] of [
    ["user-avatar-img", "user-avatar-mark"],
    ["user-portrait-img", "user-portrait-mark"],
  ]) {
    const img = $(imgId);
    const mark = $(markId);
    if (!img || !mark) continue;
    // setHidden rather than `.hidden =`: the mark is an <svg>, and SVGElement
    // does not inherit the `hidden` IDL attribute, so the assignment form sets
    // a dead JS property and leaves the mark showing behind the image. That is
    // exactly what the first browser run of this found.
    if (url) {
      img.src = url;
      setHidden(img, false);
      setHidden(mark, true);
    } else {
      // The attribute is removed rather than set empty: an empty `src` resolves
      // to the page itself and the browser reports it as a failed image.
      img.removeAttribute("src");
      setHidden(img, true);
      setHidden(mark, false);
    }
  }
}

/* The brand's rule, applied to every avatar: a configured URL is arbitrary and
 * can 404, be blocked, or not be an image. Swap to the mark rather than leaving
 * a broken-image glyph where a face should be. */
function fallbackOnError(img, mark) {
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
  const restore = $("user-avatar-img")?.getAttribute("src") || "";
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

/* Decodes, crops to the portrait's own 3:4, and re-encodes as WebP.
 *
 * Cropping here rather than leaving it to `object-fit` is what makes the
 * sidebar's 22px square and the tall portrait show the same part of the
 * picture — the top, where a face is. It also means the stored bytes are only
 * the pixels that get displayed.
 */
async function downscale(file) {
  const bitmap = await createImageBitmap(file);
  try {
    const ratio = 3 / 4;
    // The largest 3:4 window that fits inside the source, anchored at the top.
    const cropW = Math.min(bitmap.width, bitmap.height * ratio);
    const cropH = Math.min(bitmap.height, bitmap.width / ratio);
    const cropX = (bitmap.width - cropW) / 2;

    const scale = Math.min(1, MAX_EDGE / cropH);
    const outW = Math.max(1, Math.round(cropW * scale));
    const outH = Math.max(1, Math.round(cropH * scale));

    const canvas = document.createElement("canvas");
    canvas.width = outW;
    canvas.height = outH;
    const ctx = canvas.getContext("2d");
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(bitmap, cropX, 0, cropW, cropH, 0, 0, outW, outH);

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
