/* Who else is in this conversation.
 *
 * The panel exists to answer one question a shared conversation raises and
 * nothing else in the UI can: *what can these other people do here?* Thetis
 * resolves every turn as `policy(speaker) ∩ ceiling(session)`, so a read-only
 * account invited into a privileged conversation stays read-only inside it.
 * That is the property that makes inviting safe — and it is completely
 * invisible, because the transcript looks identical whoever is speaking. So
 * each row carries its account's own standing, and the panel says the rule out
 * loud rather than leaving it to be inferred.
 *
 * Everything here is an affordance, never a check. The host refuses a foreign
 * invite regardless; `invitable` comes back empty for anyone who is not the
 * owner, which is what hides the controls. Drawing from that rather than from
 * a local guess is deliberate: a control that is offered and then refused is
 * worse than one that was never there.
 */
import { el } from "../lib/dom.js";
import { store } from "../lib/store.js";
import * as rail from "./rail.js";

/** The last `participants` frame, so the tab can redraw without a round trip. */
let state = null;

/** Injected rather than imported, the way every other view here takes its
 *  sender: the connection module is app-level wiring, and a view that reached
 *  for it directly would be untestable and circular. */
let send = () => {};

export function mountParticipants(sendFn) {
  send = sendFn;
}

function ago(ms) {
  if (!ms) return "";
  const secs = Math.max(0, (Date.now() - ms) / 1000);
  if (secs < 90) return "just now";
  const mins = secs / 60;
  if (mins < 90) return `${Math.round(mins)}m ago`;
  const hours = mins / 60;
  if (hours < 36) return `${Math.round(hours)}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

function standing(readOnly) {
  return el(
    "span",
    {
      class: `participant-standing${readOnly ? " is-read-only" : ""}`,
      title: readOnly
        ? "This account is read-only, so its turns here are read-only too — whatever this conversation is otherwise allowed to do."
        : "This account can make changes, within whatever ceiling this conversation has.",
    },
    readOnly ? "read-only" : "read-write"
  );
}

function row(person, amOwner, me) {
  const isMe = person.account === me;
  // The owner cannot be removed — ownership is not participation, and there is
  // no one to hand the conversation to. Everyone else can be removed by the
  // owner, and anyone can remove themselves.
  const removable = !person.owner && (amOwner || isMe);
  return el(
    "div",
    { class: `participant${person.owner ? " is-owner" : ""}` },
    el(
      "div",
      { class: "participant-copy" },
      el(
        "div",
        { class: "participant-name" },
        person.display || person.account,
        isMe ? el("span", { class: "participant-you" }, "you") : null,
        person.owner ? el("span", { class: "participant-tag" }, "owner") : null
      ),
      el(
        "div",
        { class: "participant-meta" },
        el("span", { class: "mono" }, person.account),
        standing(person.read_only),
        person.added_by
          ? el("span", {}, `invited by ${person.added_by} ${ago(person.added_ms)}`.trim())
          : null
      )
    ),
    removable
      ? el(
          "button",
          {
            class: "participant-remove",
            title: isMe
              ? "Leave this conversation. You will no longer see it."
              : `Remove ${person.display || person.account}. Any turn of theirs still running is stopped.`,
            onclick: () => {
              send({
                type: "participant-remove",
                id: state.session,
                account: person.account,
              });
              // The removal answers with the sidebar only, because the host
              // cannot know whether the roster is still ours to read: leaving
              // ends our access to this conversation, and asking anyway is
              // refused. Here we *do* know, so ask for the fresh roster when
              // it was somebody else who left, and stay quiet when it was us.
              if (!isMe) {
                send({ type: "participants", id: state.session });
              }
            },
          },
          isMe ? "Leave" : "Remove"
        )
      : null
  );
}

/** The invite control: a picker of accounts, drawn only when the host offered
 *  any. Not a free-text box — an id typed by hand is a guess, and the useful
 *  failure ("no such account") is one the picker cannot make. */
function invite(candidates) {
  const select = el(
    "select",
    { class: "participant-picker" },
    el("option", { value: "" }, "Choose an account…"),
    ...candidates.map((a) =>
      el(
        "option",
        { value: a.id },
        `${a.display || a.id}${a.read_only ? " · read-only" : ""}`
      )
    )
  );
  const button = el(
    "button",
    {
      class: "participant-invite",
      onclick: () => {
        if (!select.value) return;
        send({
          type: "participant-add",
          id: state.session,
          account: select.value,
        });
        select.value = "";
      },
    },
    "Invite"
  );
  return el("div", { class: "participant-invite-row" }, select, button);
}

export function openTab() {
  const data = state && state.session === store.current ? state : null;
  const people = data?.participants || [];
  const me = store.user?.id || "";
  const amOwner = people.some((p) => p.owner && p.account === me);
  const candidates = data?.invitable || [];

  const blocks = [];
  if (!store.current) {
    blocks.push(el("div", { class: "panel-note" }, "Open a conversation first."));
  } else if (!data) {
    blocks.push(el("div", { class: "panel-note" }, "Loading…"));
  } else {
    blocks.push(...people.map((p) => row(p, amOwner, me)));
    if (candidates.length) blocks.push(invite(candidates));
    // Said once, at the bottom, because it is the whole point of the panel and
    // the thing a reader would otherwise get wrong: an invitation shares the
    // conversation, not the owner's permissions.
    blocks.push(
      el(
        "p",
        { class: "panel-section-note" },
        amOwner && candidates.length === 0 && store.user?.local
          ? "Accounts are off, so there is nobody to invite. Turn on auth.mode = \"users\" to share a conversation."
          : "Everyone here reads the same transcript and can speak in it. What a turn may actually do is decided by whoever sent it — an invitation never lends out your own permissions."
      )
    );
  }

  rail.open({
    id: "participants",
    title: "People",
    subtitle:
      people.length > 1
        ? `${people.length} people`
        : people.length === 1
          ? "Just you"
          : "",
    blocks,
  });
}

export function onFrame(frame) {
  state = frame;
  if (rail.isOpen("participants")) openTab();
}

/** Asks for the roster. Called when the tab opens and after switching
 *  conversations, since the frame is per-conversation. */
export function request() {
  if (store.current) send({ type: "participants", id: store.current });
}
