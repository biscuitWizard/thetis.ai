/* The composer: text, attachments, the mode and model pickers, and the stop
 * button that appears while a turn is running. */

import { $, AGENT_NAME, clear, el, icon } from "../lib/dom.js";
import { store } from "../lib/store.js";
import { toast } from "../lib/toast.js";
import { Picker } from "./picker.js";

const X = ["M4 4l8 8", "M12 4l-8 8"];

/** Images only, and small enough that base64 in a websocket frame is sane. */
const MAX_BYTES = 8 * 1024 * 1024;

export function mountComposer({ onSend, onSetMode, onSetModel, onSetBase, onShowHistory, onStop }) {
  const form = $("composer");
  const input = $("input");
  const sendBtn = $("send");
  const stopBtn = $("stop");
  const tray = $("attachments");
  const fileInput = $("file-input");
  const veil = $("drop-veil");

  // The stop control lives where the eyes already are while a turn runs. It
  // shows on busy and asks no confirmation: stopping a turn is not
  // destructive — the log keeps everything the turn already did.
  stopBtn.addEventListener("click", () => onStop());
  store.watch("busy", (busy) => {
    stopBtn.hidden = !busy;
  });
  stopBtn.hidden = !store.busy;

  // --- pickers --------------------------------------------------------------

  const modePicker = new Picker($("mode-picker"), {
    title: `How ${AGENT_NAME} should work in this conversation`,
    options: () => store.modes.map((m) => ({ id: m.id, label: m.label, note: m.description })),
    selected: () => store.mode,
    render: () => store.modeLabel(),
    dotClass: (id) => (id === "plan" ? "is-plan" : ""),
    onSelect: onSetMode,
  });

  const modelPicker = new Picker($("model-picker"), {
    title: "Model for this conversation. Edit the list under Models.",
    options: () => {
      const options = [
        { id: "", label: "Default model", note: "Whatever the grip is configured with." },
        ...store.models.map((m) => ({ id: m.id, label: m.label, note: m.id })),
      ];
      // A conversation can name a model the catalogue no longer offers. Listing
      // it keeps the menu honest about what is selected, rather than showing
      // nothing as chosen while the turns run on it.
      if (store.model && !options.some((o) => o.id === store.model)) {
        options.push({ id: store.model, label: store.modelLabel(), note: `${store.model} · not listed` });
      }
      return options;
    },
    selected: () => store.model,
    render: () => store.modelLabel(),
    onSelect: onSetModel,
  });

  // The starting-point selector. Before the first message it is a picker over
  // trunk's history — the revision the sandbox branch will fork from. After
  // the first message pins the branch, the same mount becomes the branch
  // indicator: position against trunk, clicking opens the history view.
  const revisionMount = $("revision-picker");
  const revisionPicker = new Picker(revisionMount, {
    title: "Which trunk revision this conversation starts from",
    options: () => [
      { id: "", label: "Latest trunk", note: "Whatever trunk holds when the first message is sent." },
      ...store.trunkLog.map((c) => ({
        id: c.rev,
        label: `${c.rev.slice(0, 8)} · ${c.subject.slice(0, 40)}`,
        note: c.author,
      })),
    ],
    selected: () => store.baseRevision,
    render: () => `Start from: ${store.baseRevisionLabel()}`,
    onSelect: onSetBase,
  });

  function drawRevisionMount() {
    const branch = store.branch;
    if (branch?.materialized) {
      // Indicator mode: a plain button in the picker's clothes.
      const conflicted = branch.state === "conflict";
      const bits = [branch.branch];
      if (branch.ahead > 0) bits.push(`↑${branch.ahead}`);
      if (branch.behind > 0) bits.push(`↓${branch.behind}`);
      clear(revisionMount).append(
        el(
          "button",
          {
            type: "button",
            class: "picker-btn",
            title: "This conversation's sandbox branch — click for its history",
            onClick: onShowHistory,
          },
          el("span", { class: `picker-dot${conflicted ? " is-err" : ""}` }),
          el("span", { class: "picker-label mono" }, bits.join(" "))
        )
      );
      revisionMount.className = "picker";
    } else if (store.current && !store.hasMessages && store.trunkLog.length) {
      revisionPicker.refresh();
      revisionPicker.draw();
    } else {
      // No conversation, or an older host that never sends branch frames.
      clear(revisionMount);
    }
  }

  store.watch("branch", drawRevisionMount);
  store.watch("hasMessages", drawRevisionMount);
  store.watch("trunkLog", drawRevisionMount);
  store.watch("baseRevision", drawRevisionMount);
  store.watch("current", drawRevisionMount);
  drawRevisionMount();

  // The Models panel can add, relabel or hide an entry, so the picker follows
  // both lists rather than only the visible one.
  store.watch("modelsHidden", () => modelPicker.refresh());

  store.watch("modes", () => modePicker.refresh());
  store.watch("models", () => modelPicker.refresh());
  store.watch("mode", () => modePicker.refresh());
  store.watch("model", () => modelPicker.refresh());

  // --- attachments ----------------------------------------------------------

  function drawTray() {
    clear(tray);
    tray.hidden = store.attachments.length === 0;

    store.attachments.forEach((file, index) => {
      tray.append(
        el(
          "div",
          { class: "chip", title: file.name },
          file.mime.startsWith("image/")
            ? el("img", { src: `data:${file.mime};base64,${file.data}`, alt: "" })
            : null,
          el("span", { class: "chip-name" }, file.name),
          el(
            "button",
            {
              type: "button",
              class: "chip-x",
              title: "Remove",
              "aria-label": `Remove ${file.name}`,
              onClick: () => {
                store.attachments.splice(index, 1);
                store.touch("attachments");
              },
            },
            icon(X, { size: 11, width: 1.9 })
          )
        )
      );
    });
    updateSendState();
  }

  store.watch("attachments", drawTray);

  async function addFiles(files) {
    for (const file of files) {
      if (!file.type.startsWith("image/")) continue;
      if (file.size > MAX_BYTES) {
        toast(`${file.name} is too large (limit ${Math.round(MAX_BYTES / 1024 / 1024)} MB).`, { tone: "error" });
        continue;
      }
      store.attachments.push({
        name: file.name || "pasted-image.png",
        mime: file.type,
        data: await toBase64(file),
      });
    }
    store.touch("attachments");
  }

  function toBase64(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      // The result is a data URL; the payload is everything after the comma.
      reader.onload = () => resolve(String(reader.result).split(",", 2)[1] || "");
      reader.onerror = reject;
      reader.readAsDataURL(file);
    });
  }

  $("attach").addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", async () => {
    await addFiles([...fileInput.files]);
    fileInput.value = "";
  });

  input.addEventListener("paste", (event) => {
    const files = [...(event.clipboardData?.files || [])];
    if (files.length) {
      event.preventDefault();
      addFiles(files);
    }
  });

  // --- drag and drop --------------------------------------------------------

  // The veil is held open by a timer that every `dragover` refreshes, rather
  // than by counting dragenter/dragleave pairs. Those fire once per child
  // element and go missing entirely when a drag ends outside the window, which
  // strands the overlay on screen. A lapsing timer cannot get stuck: the moment
  // events stop arriving, the veil clears itself.
  const VEIL_LINGER_MS = 160;
  let veilTimer = null;

  const draggingFiles = (event) =>
    [...(event.dataTransfer?.types || [])].includes("Files");

  /* Another surface may own this drag. The Files tab marks its drop zones with
   * `data-drop-into`, and a file dragged there is destined for the workspace,
   * not for the message being typed — so the veil must stay down and the drop
   * must not also attach. Those zones stop their own drop from bubbling here,
   * but `dragover` is checked as well: otherwise the veil would flash over the
   * conversation, telling the user the wrong thing about where the file lands. */
  const ownedElsewhere = (event) =>
    event.target instanceof Element && !!event.target.closest("[data-drop-into]");

  function holdVeil() {
    veil.hidden = false;
    clearTimeout(veilTimer);
    veilTimer = setTimeout(dropVeil, VEIL_LINGER_MS);
  }

  function dropVeil() {
    clearTimeout(veilTimer);
    veilTimer = null;
    veil.hidden = true;
  }

  // Bound to the window so a drop anywhere attaches, and so a file dropped
  // outside the composer never navigates the page away.
  window.addEventListener("dragover", (event) => {
    if (!draggingFiles(event)) return;
    // Still prevented: without it the browser would navigate away on the drop
    // even though this surface is not the one attaching the file.
    event.preventDefault();
    if (ownedElsewhere(event)) {
      dropVeil();
      return;
    }
    holdVeil();
  });

  window.addEventListener("drop", async (event) => {
    if (!draggingFiles(event)) return;
    event.preventDefault();
    dropVeil();
    if (ownedElsewhere(event)) return;
    await addFiles([...(event.dataTransfer?.files || [])]);
  });

  // Belt and braces for the cases the timer would only catch a beat later.
  window.addEventListener("dragend", dropVeil);
  window.addEventListener("blur", dropVeil);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) dropVeil();
  });

  // --- sending --------------------------------------------------------------

  /* The composer is locked while a submission is in flight, or while a
   * conversation is being created. Both are host round trips that take long
   * enough to type into, and a second Enter during the first one used to send a
   * message the user had no feedback about. */
  function locked() {
    return Boolean(store.pending || store.creating);
  }

  function updateSendState() {
    const hasContent = input.value.trim() !== "" || store.attachments.length > 0;
    sendBtn.disabled = !hasContent || !store.current || locked();
  }

  function drawLock() {
    const busy = locked();
    input.disabled = busy;
    input.placeholder = busy
      ? store.creating
        ? "Creating the conversation…"
        : "Sending…"
      : `Message ${AGENT_NAME}…`;
    form.classList.toggle("is-locked", busy);
    $("attach").disabled = busy;
    updateSendState();
    // Focus comes back by itself when the lock lifts, so typing can continue
    // where it left off rather than after a click.
    if (!busy && document.activeElement === document.body) input.focus();
  }

  store.watch("pending", drawLock);
  store.watch("creating", drawLock);

  function autosize() {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, window.innerHeight * 0.4)}px`;
  }

  input.addEventListener("input", () => {
    autosize();
    updateSendState();
  });

  // Enter sends. Sending mid-reply is allowed on purpose: the orchestrator
  // turns it into a nudge for the running turn rather than a second turn.
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      form.requestSubmit();
    }
  });

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (locked()) return;
    if (!store.current || (!text && !store.attachments.length)) return;

    const attachments = store.attachments.slice();
    // `onSend` reports whether the frame actually reached the socket. A send
    // into a closed socket used to swallow the message silently, clearing the
    // box as if it had gone.
    if (onSend(text, attachments) === false) {
      toast("Not connected — the message was not sent. It is still in the box.", {
        tone: "error",
      });
      return;
    }

    input.value = "";
    store.attachments.length = 0;
    store.touch("attachments");
    autosize();
  });

  /** Puts an unacknowledged message back in the box, so nothing is lost. */
  function restore(text, attachments) {
    if (text && !input.value.trim()) input.value = text;
    if (attachments?.length && !store.attachments.length) {
      store.attachments.push(...attachments);
      store.touch("attachments");
    }
    autosize();
    drawLock();
    input.focus();
  }

  store.watch("current", updateSendState);
  drawTray();
  drawLock();
  return { focus: () => input.focus(), restore };
}
