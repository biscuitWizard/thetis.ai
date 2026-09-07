/* The `ask_user` form: questions the agent asked, answered in the transcript.
 *
 * This is the one interactive element that lives *inside* the conversation
 * rather than in the rail. That is deliberate and is not a modal by the back
 * door: the questions are a message, they belong in the reading order of the
 * messages around them, and answering them is answering the agent. A rail tab
 * would separate the question from the sentence that motivated it, and a modal
 * would block the conversation the user may need to re-read in order to reply.
 *
 * The form is built from the tool call's own arguments, so it needs no extra
 * wire frame; answers go back as an ordinary user message through the composer's
 * send path. That is what makes it replay-safe — the answers are in the event
 * log as text, exactly as if they had been typed.
 */

import { AGENT_NAME, el, clear } from "../lib/dom.js";

/** What a question's free-text option is labelled, on every surface. */
const OTHER_LABEL = "Something else…";

/** Reads the tool call's arguments into the shape this view draws. */
export function parseAsk(raw) {
  let value;
  try {
    value = JSON.parse(raw || "{}");
  } catch {
    return null;
  }
  const questions = Array.isArray(value?.questions) ? value.questions : [];
  if (!questions.length) return null;

  return {
    intro: typeof value.intro === "string" ? value.intro.trim() : "",
    questions: questions
      .map((q, index) => {
        const question = typeof q?.question === "string" ? q.question.trim() : "";
        if (!question) return null;
        const options = Array.isArray(q?.options)
          ? q.options.filter((o) => typeof o === "string" && o.trim()).map((o) => o.trim())
          : [];
        // The kind is inferred the same way the tool infers it, so a call that
        // omitted `type` draws as the tool described it.
        const kind = q?.type === "choice" || q?.type === "open"
          ? q.type
          : options.length
            ? "choice"
            : "open";
        return {
          key: typeof q?.id === "string" && q.id.trim() ? q.id.trim() : String(index + 1),
          question,
          kind,
          options: kind === "choice" ? options : [],
          multiple: q?.allow_multiple === true,
        };
      })
      .filter(Boolean),
  };
}

/* One question block.
 *
 * Every choice question ends with a free-text option, and every question can be
 * skipped: those two controls are added here rather than by the caller, so no
 * surface can accidentally offer a question the user cannot escape.
 */
function questionBlock(q, index, state, onChange) {
  const name = `ask-${state.formId}-${index}`;
  const answer = state.answers[index];

  const otherInput = el("input", {
    type: "text",
    class: "ask-other-input",
    placeholder: "Your own answer",
    value: answer.other || "",
    hidden: !answer.otherPicked,
    onInput: (event) => {
      answer.other = event.target.value;
      onChange();
    },
  });

  function pick(option, checked) {
    if (q.multiple) {
      const set = new Set(answer.picked);
      if (checked) set.add(option);
      else set.delete(option);
      answer.picked = [...set];
    } else {
      answer.picked = checked ? [option] : [];
      answer.otherPicked = false;
      otherInput.hidden = true;
    }
    answer.skipped = false;
    onChange();
  }

  const controls = q.kind === "choice"
    ? [
        ...q.options.map((option) =>
          el(
            "label",
            { class: "ask-option" },
            el("input", {
              type: q.multiple ? "checkbox" : "radio",
              name,
              checked: answer.picked.includes(option),
              onChange: (event) => pick(option, event.target.checked),
            }),
            el("span", {}, option)
          )
        ),
        // Always last, always present. A choice list the agent wrote is a guess
        // about the answer space; this is how the user disagrees with it.
        el(
          "label",
          { class: "ask-option is-other" },
          el("input", {
            type: q.multiple ? "checkbox" : "radio",
            name,
            checked: answer.otherPicked,
            onChange: (event) => {
              answer.otherPicked = event.target.checked;
              if (!q.multiple) answer.picked = [];
              if (event.target.checked) answer.skipped = false;
              otherInput.hidden = !event.target.checked;
              if (event.target.checked) otherInput.focus();
              onChange();
            },
          }),
          el("span", {}, OTHER_LABEL)
        ),
        otherInput,
      ]
    : [
        // A textarea's content is its child text, not a `value` attribute — the
        // attribute is silently ignored, which would lose a restored draft.
        el(
          "textarea",
          {
            class: "ask-text",
            rows: "2",
            placeholder: "Your answer",
            onInput: (event) => {
              answer.text = event.target.value;
              if (event.target.value.trim()) answer.skipped = false;
              onChange();
            },
          },
          answer.text || ""
        ),
      ];

  const skip = el(
    "button",
    {
      type: "button",
      class: `ask-skip${answer.skipped ? " is-on" : ""}`,
      title: "Leave this question unanswered",
      onClick: () => {
        answer.skipped = !answer.skipped;
        if (answer.skipped) {
          answer.picked = [];
          answer.otherPicked = false;
          answer.other = "";
          answer.text = "";
          otherInput.hidden = true;
          // The DOM holds the checked state, so it has to be cleared too.
          block.querySelectorAll("input[type=radio],input[type=checkbox]").forEach((i) => {
            i.checked = false;
          });
          const area = block.querySelector(".ask-text");
          if (area) area.value = "";
        }
        skip.classList.toggle("is-on", answer.skipped);
        block.classList.toggle("is-skipped", answer.skipped);
        onChange();
      },
    },
    "Skip"
  );

  const block = el(
    "div",
    { class: `ask-q${answer.skipped ? " is-skipped" : ""}` },
    el(
      "div",
      { class: "ask-q-head" },
      el("span", { class: "ask-q-num" }, String(index + 1)),
      el("span", { class: "ask-q-text" }, q.question),
      skip
    ),
    el("div", { class: "ask-controls" }, ...controls)
  );

  return block;
}

/** What one answer contributes to the message, or null when left out. */
function renderAnswer(q, answer) {
  if (answer.skipped) return "(skipped)";
  const parts = [...answer.picked];
  if (answer.otherPicked && answer.other.trim()) parts.push(answer.other.trim());
  if (q.kind === "open") {
    const text = (answer.text || "").trim();
    return text || null;
  }
  return parts.length ? parts.join(", ") : null;
}

/* The message the answers become.
 *
 * Written as prose with the questions restated, because the model reads this as
 * an ordinary user message: it has no access to the form's structure, so an
 * answer that does not name its question is an answer it has to guess at.
 */
export function composeAnswers(ask, state) {
  const lines = [];
  let answered = 0;
  ask.questions.forEach((q, index) => {
    const value = renderAnswer(q, state.answers[index]);
    if (value === null) {
      lines.push(`${index + 1}. ${q.question}\n   (no answer)`);
      return;
    }
    if (value !== "(skipped)") answered += 1;
    lines.push(`${index + 1}. ${q.question}\n   ${value}`);
  });
  return { text: `Answers:\n\n${lines.join("\n")}`, answered };
}

/* Builds the card.
 *
 * `onAnswer(text)` sends the answers as a user message and returns whether the
 * socket took it — the same contract the composer uses, so a send into a dead
 * socket keeps the form usable instead of silently losing what was typed.
 */
export function askCard(ask, { onAnswer, answered = false } = {}) {
  const state = {
    formId: Math.random().toString(36).slice(2, 8),
    answers: ask.questions.map(() => ({
      picked: [],
      otherPicked: false,
      other: "",
      text: "",
      skipped: false,
    })),
  };

  const send = el(
    "button",
    { type: "button", class: "ghost-btn is-primary", disabled: true, onClick: submit },
    "Send answers"
  );
  const note = el("div", { class: "ask-note" }, "");

  function refresh() {
    const { answered: count } = composeAnswers(ask, state);
    const touched = state.answers.some(
      (a) => a.skipped || a.picked.length || (a.otherPicked && a.other.trim()) || (a.text || "").trim()
    );
    send.disabled = !touched;
    const total = ask.questions.length;
    note.textContent = touched
      ? `${count} of ${total} answered — the rest go back as skipped.`
      : `Answer what you can. Anything you leave is sent as skipped.`;
  }

  function lock(message) {
    card.classList.add("is-answered");
    card.querySelectorAll("input, textarea, button").forEach((node) => {
      node.disabled = true;
    });
    clear(foot).append(el("div", { class: "ask-note" }, message));
  }

  function submit() {
    const { text } = composeAnswers(ask, state);
    if (onAnswer?.(text) === false) return;
    lock("Answers sent.");
  }

  const foot = el(
    "div",
    { class: "ask-foot" },
    send,
    el(
      "button",
      {
        type: "button",
        class: "ghost-btn",
        title: "Send every question back unanswered",
        onClick: () => {
          state.answers.forEach((a) => {
            a.skipped = true;
            a.picked = [];
            a.otherPicked = false;
            a.other = "";
            a.text = "";
          });
          const { text } = composeAnswers(ask, state);
          if (onAnswer?.(text) === false) return;
          lock("Skipped every question.");
        },
      },
      "Skip all"
    ),
    note
  );

  const card = el(
    "div",
    { class: "ask" },
    el(
      "div",
      { class: "ask-head" },
      el("span", { class: "ask-mark" }, "?"),
      el(
        "div",
        {},
        el("div", { class: "ask-title" }, `${AGENT_NAME} is asking`),
        ask.intro ? el("div", { class: "ask-intro" }, ask.intro) : null
      )
    ),
    el(
      "div",
      { class: "ask-body" },
      ...ask.questions.map((q, index) => questionBlock(q, index, state, refresh))
    ),
    foot
  );

  refresh();
  // A replayed transcript shows the form as it was left: the answers are
  // already in the log below it, so offering the controls again would invite
  // answering twice.
  if (answered) lock("Already answered — see the reply below.");
  return card;
}
