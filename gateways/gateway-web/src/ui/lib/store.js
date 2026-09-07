/* Client state.
 *
 * One object, one change notification. Views subscribe to what they care about
 * and re-render; nothing mutates the DOM behind the store's back.
 */

export const store = {
  sessions: [],
  current: null,
  title: "",
  mode: "",
  model: "",
  models: [],
  /** Models pushed out of the picker; the inspector still lists them so one
   *  can be brought back. */
  modelsHidden: [],
  modelsRestricted: false,
  user: null,
  modes: [],
  busy: false,
  attachments: [],
  /* A submitted message that the host has not echoed back yet, or null.
   *
   * The transcript is a function of the event log, and the log says nothing
   * until `submit` returns — which for a conversation's first message means
   * after a branch, a worktree and a worker have been created, several seconds
   * later. Without this the UI showed an empty composer over "No messages yet"
   * and looked like the send had been a no-op. Shape:
   * {text, attachments, first}. */
  pending: null,
  /** True between the host acknowledging a message and its `user` event
   *  arriving: the composer is free again, but the optimistic row still stands
   *  in for a message the log has not yet echoed. */
  awaitingEcho: false,
  /** True between clicking + and the new conversation's history arriving. */
  creating: false,
  skills: [],
  tools: [],
  /** Authoritative todo frame for the current conversation. */
  todos: null,

  /* Sub-agents of the conversation on screen, in spawn order.
   *
   * Shape: {id, label, state, cost}, where state is "running" | "done" |
   * whatever a failed turn stopped by. Published by the transcript, because the
   * transcript is what learns of a child at all: children are deliberately
   * absent from `list_sessions` — as top-level rows they would look like
   * conversations you could talk into, and opening one would give a chat with
   * no composer — so the only signal is their tagged frames.
   *
   * The consequence, worth knowing: this only ever describes the *current*
   * conversation. A child running in a conversation you are not watching is not
   * known to this client at all. */
  agents: [],

  /** The current conversation's branch-status frame, or null (panel hidden). */
  branch: null,
  /** Commits for the History view. */
  branchLog: [],
  /** What the rail's Branch tab is showing: the live graph, or history. */
  branchView: "graph",
  /** The two-rail commit graph the branch panel draws. */
  branchGraph: null,
  /** Trunk commits offered by the composer's starting-point picker. */
  trunkLog: [],
  /** Revision chosen before the first message pins the branch ("" = latest). */
  baseRevision: "",
  /** Whether this conversation has any user message yet — gates the
   *  starting-point picker (before) vs the branch indicator (after). */
  hasMessages: false,

  /** The session's usage ledger, one entry per finished turn. */
  turnStats: [],
  /** The turn in flight, accumulated from each assistant message's usage, or
   *  null between turns. A turn here can run for dozens of steps and several
   *  dollars, so waiting for turn-finished to show any of it reports nothing
   *  during exactly the part worth watching. */
  liveTurn: null,
  /** Sum of turnStats costs, for the header's spend chip. */
  spendSession: 0,
  /** The last assistant message's usage record and model, for the Usage view. */
  lastUsage: null,
  lastModel: "",

  /** The host's last system-status frame — trunk, the served UI build, the
   *  worker fleet, the machine — for the status bar along the foot. Global
   *  rather than per-conversation: it describes the installation. */
  system: null,

  _watchers: new Map(),

  /** Subscribes to one key. Returns an unsubscribe function. */
  watch(key, fn) {
    if (!this._watchers.has(key)) this._watchers.set(key, new Set());
    this._watchers.get(key).add(fn);
    return () => this._watchers.get(key).delete(fn);
  },

  /** Applies a patch and notifies watchers of the keys that actually changed. */
  set(patch) {
    const touched = [];
    for (const [key, value] of Object.entries(patch)) {
      if (this[key] === value) continue;
      this[key] = value;
      touched.push(key);
    }
    for (const key of touched) {
      this._watchers.get(key)?.forEach((fn) => fn(this[key], this));
    }
    return touched;
  },

  /** Forces watchers to run even when the reference is unchanged. */
  touch(...keys) {
    for (const key of keys) {
      this._watchers.get(key)?.forEach((fn) => fn(this[key], this));
    }
  },

  modeLabel() {
    return this.modes.find((m) => m.id === this.mode)?.label || "Agent";
  },

  baseRevisionLabel() {
    if (!this.baseRevision) return "Latest trunk";
    const known = this.trunkLog.find((c) => c.rev === this.baseRevision);
    const short = this.baseRevision.slice(0, 8);
    return known ? `${short} · ${known.subject.slice(0, 24)}` : short;
  },

  modelLabel() {
    if (!this.model) return "Default model";
    // Falls back to the raw slug on purpose. A conversation can name a model
    // that is not in the catalogue - one hidden since, or set before it was
    // listed - and showing the slug is honest where "Default model" would be a
    // lie about what the turn will actually use.
    const known = [...this.models, ...this.modelsHidden].find((m) => m.id === this.model);
    return known?.label || this.model;
  },
};
