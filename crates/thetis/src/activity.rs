//! What every conversation is doing right now, for the sidebar.
//!
//! The transcript learns a conversation's state from its event stream, but a
//! browser tab only receives the stream of the conversation it is watching:
//! `web.rs` fans a frame out to the sockets subscribed to that session and to
//! nobody else. So a tab looking at conversation A had no way to know that B
//! was mid-turn, that C had stopped to ask a question, or that D failed —
//! the sidebar rows for all three stood still until they were opened.
//!
//! This module is the fix. Every rendered frame the workers send passes
//! through [`Activity::note`] on its way to the broadcast channel, which folds
//! it into one small snapshot per conversation: working or waiting or idle,
//! what the current step is (thinking, writing, or a tool by name), how many
//! steps and dollars the turn has taken so far, and how many sub-agents are
//! running. A snapshot that *changed* is published on a second broadcast, and
//! `web.rs` pushes it to every socket whose principal may see the session,
//! watched or not. The `sessions` list is decorated with the same snapshots so
//! a fresh tab is right on its first paint.
//!
//! Deliberately derived from the frames rather than asked of the workers: a
//! worker is the shortest-lived thing in the system, and this has to answer
//! for a conversation whose worker was reaped a moment ago just as well as
//! for one mid-tool. It is in-memory only; after a restart every conversation
//! reads as idle, which is also true.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;
use tokio::sync::broadcast;

/// The tool whose call ends a turn by putting questions to the user. A turn
/// that stopped this way is waiting on a person, which the sidebar shows
/// differently from a turn that simply finished.
const ASK_TOOL: &str = "ask_user";

/// A conversation's live state. Serialized straight onto the wire as the body
/// of an `activity` frame and as the `activity` field of a `sessions` row, so
/// field names here are the protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Snapshot {
    /// `working` | `waiting` | `failed` | `idle`.
    pub state: &'static str,
    /// What the agent is doing this instant, while working: `starting`,
    /// `thinking`, `writing`, `compacting`, or a tool's name. Empty when idle.
    pub step: String,
    /// Tool calls so far this turn.
    pub steps: u32,
    /// Spend so far this turn, in dollars, accumulated from each assistant
    /// message's usage — a turn can run for dozens of steps before it ends.
    pub cost: f64,
    /// Sub-agents currently mid-turn.
    pub agents: u32,
    /// When the current state began (the frame's own clock, ms since epoch).
    pub since_ms: u64,
    /// How the last turn stopped (`stop`, `asked`, `cancelled`, an error…).
    /// Empty until a turn has finished.
    pub outcome: String,
    /// Bumped on every published change. A pushed `activity` frame and a
    /// `sessions` reply travel on different paths and can cross, so the client
    /// keeps whichever of the two is newer by this rather than by arrival.
    pub rev: u64,
}

impl Snapshot {
    fn idle() -> Self {
        Self {
            state: "idle",
            ..Self::default()
        }
    }

    /// Folds one rendered frame in. Returns whether anything visible moved,
    /// so a stream of deltas — hundreds per reply — publishes once, when the
    /// step flips to `writing`, and never again until something else happens.
    fn apply(&mut self, frame: &Value) -> bool {
        let before = self.clone();
        let kind = frame.get("kind").and_then(Value::as_str).unwrap_or_default();
        let ts = frame.get("ts").and_then(Value::as_u64).unwrap_or_else(crate::store::now_ms);
        let is_child = frame.get("agent").is_some();

        if is_child {
            // A child's frames are re-addressed to the parent and carry an
            // `agent` tag. Only its turn boundaries matter here: its tools and
            // tokens are its own business, and the parent's step already names
            // the tool that is waiting on it.
            match kind {
                "turn-started" => self.agents = self.agents.saturating_add(1),
                "turn-finished" => self.agents = self.agents.saturating_sub(1),
                _ => {}
            }
            return *self != before;
        }

        match kind {
            // The message is in the log; the turn follows within moments. Light
            // the row now rather than after the worker has spun up, which for a
            // fresh conversation is a branch, a worktree and a process.
            "user" => {
                if self.state != "working" {
                    self.begin(ts);
                }
            }
            "turn-started" => {
                // Already lit by the user message; only start the clock if not.
                if self.state != "working" {
                    self.begin(ts);
                }
                self.step = "thinking".into();
            }
            "reasoning" => self.working_step("thinking"),
            "delta" => self.working_step("writing"),
            "compacting" => self.working_step("compacting"),
            "tool-call" => {
                let name = frame.get("name").and_then(Value::as_str).unwrap_or("tool");
                self.working_step(name);
                self.steps = self.steps.saturating_add(1);
            }
            "assistant" => {
                if let Some(cost) = frame.pointer("/usage/cost").and_then(Value::as_f64) {
                    self.cost += cost;
                }
            }
            "turn-finished" => {
                let stopped_by = frame
                    .get("stopped_by")
                    .and_then(Value::as_str)
                    .unwrap_or("stop");
                // A turn that ended in `ask_user` is waiting on a person, and
                // the sidebar should say so: it is the one state where the
                // conversation is blocked on the reader rather than the agent.
                let asked = stopped_by == "asked"
                    || (stopped_by == "stop" && self.step == ASK_TOOL);
                self.state = match stopped_by {
                    _ if asked => "waiting",
                    "stop" | "cancelled" | "restarted" | "" => "idle",
                    _ => "failed",
                };
                self.step.clear();
                self.since_ms = ts;
                self.outcome = if asked { "asked".into() } else { stopped_by.to_string() };
                // Children still running after the parent stopped keep their
                // count; their own turn-finished frames retire them.
            }
            _ => {}
        }
        *self != before
    }

    fn begin(&mut self, ts: u64) {
        self.state = "working";
        self.step = "starting".into();
        self.steps = 0;
        self.cost = 0.0;
        self.since_ms = ts;
        self.outcome.clear();
    }

    /// Sets the step, and puts a conversation back to work if a frame arrives
    /// for a turn this process never saw start — a gateway restarted mid-turn,
    /// or a broadcast lagged.
    fn working_step(&mut self, step: &str) {
        if self.state != "working" {
            self.begin(crate::store::now_ms());
        }
        if self.step != step {
            self.step = step.to_string();
        }
    }
}

/// One change, as published: which conversation, and its whole new snapshot.
#[derive(Debug, Clone)]
pub struct Change {
    pub session_id: String,
    pub snapshot: Snapshot,
}

pub struct Activity {
    by_session: RwLock<HashMap<String, Snapshot>>,
    tx: broadcast::Sender<Change>,
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    pub fn new() -> Self {
        // Sized for bursts: a turn with sub-agents can flip several snapshots
        // in one second. A lagging socket just misses intermediate states and
        // catches up on its next `sessions` list.
        let (tx, _) = broadcast::channel(256);
        Self {
            by_session: RwLock::new(HashMap::new()),
            tx,
        }
    }

    /// Folds a rendered frame into its conversation's snapshot and publishes
    /// the result if it changed. Anything that is not an `event` frame — a
    /// terminal feed, a branch result — is ignored.
    pub fn note(&self, session_id: &str, frame: &str) {
        // Cheap reject before parsing: only event frames carry a kind.
        if !frame.contains("\"kind\"") {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(frame) else {
            return;
        };
        if value.get("type").and_then(Value::as_str) != Some("event") {
            return;
        }
        self.note_value(session_id, &value);
    }

    fn note_value(&self, session_id: &str, value: &Value) {
        let changed = {
            let mut map = match self.by_session.write() {
                Ok(m) => m,
                Err(poisoned) => poisoned.into_inner(),
            };
            let snap = map
                .entry(session_id.to_string())
                .or_insert_with(Snapshot::idle);
            snap.apply(value).then(|| {
                snap.rev += 1;
                snap.clone()
            })
        };
        if let Some(snapshot) = changed {
            let _ = self.tx.send(Change {
                session_id: session_id.to_string(),
                snapshot,
            });
        }
    }

    /// The snapshot for one conversation, if any frame has been seen for it.
    pub fn get(&self, session_id: &str) -> Option<Snapshot> {
        self.by_session
            .read()
            .ok()
            .and_then(|m| m.get(session_id).cloned())
    }

    /// Every conversation that is not simply idle, for decorating a list.
    pub fn all(&self) -> HashMap<String, Snapshot> {
        self.by_session
            .read()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.tx.subscribe()
    }

    /// The `activity` wire frame for one change.
    pub fn frame(change: &Change) -> String {
        let mut body = serde_json::to_value(&change.snapshot).unwrap_or(Value::Null);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("type".into(), Value::from("activity"));
            obj.insert("session".into(), Value::from(change.session_id.as_str()));
        }
        body.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, extra: Value) -> String {
        let mut v = json!({ "type": "event", "session": "s", "seq": 1, "ts": 1000, "kind": kind });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                o.insert(k.clone(), val.clone());
            }
        }
        v.to_string()
    }

    #[test]
    fn a_turn_lights_on_the_user_message_and_names_each_step() {
        let a = Activity::new();
        let mut rx = a.subscribe();
        a.note("s", &ev("user", json!({})));
        let snap = a.get("s").unwrap();
        assert_eq!(snap.state, "working");
        assert_eq!(snap.step, "starting");
        assert!(rx.try_recv().is_ok(), "the transition was published");

        a.note("s", &ev("turn-started", json!({})));
        assert_eq!(a.get("s").unwrap().step, "thinking");
        a.note("s", &ev("delta", json!({"text": "hel"})));
        a.note("s", &ev("delta", json!({"text": "lo"})));
        assert_eq!(a.get("s").unwrap().step, "writing");
        a.note("s", &ev("tool-call", json!({"name": "web-search", "id": "c1", "arguments": "{}"})));
        let snap = a.get("s").unwrap();
        assert_eq!(snap.step, "web-search");
        assert_eq!(snap.steps, 1);
        a.note("s", &ev("assistant", json!({"text": "x", "usage": {"cost": 0.25}})));
        assert!((a.get("s").unwrap().cost - 0.25).abs() < 1e-9);
    }

    #[test]
    fn every_published_change_carries_a_higher_rev() {
        let a = Activity::new();
        a.note("s", &ev("turn-started", json!({})));
        let r1 = a.get("s").unwrap().rev;
        a.note("s", &ev("delta", json!({"text": "."})));
        let r2 = a.get("s").unwrap().rev;
        a.note("s", &ev("delta", json!({"text": "."})));
        let r3 = a.get("s").unwrap().rev;
        assert!(r2 > r1);
        assert_eq!(r3, r2, "an unchanged snapshot does not bump");
    }

    #[test]
    fn deltas_publish_once() {
        let a = Activity::new();
        let mut rx = a.subscribe();
        a.note("s", &ev("turn-started", json!({})));
        for _ in 0..50 {
            a.note("s", &ev("delta", json!({"text": "."})));
        }
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        // turn-started, then the single flip to `writing`.
        assert_eq!(n, 2);
    }

    #[test]
    fn a_finished_turn_is_idle_an_asked_one_waits_and_an_error_fails() {
        let a = Activity::new();
        a.note("s", &ev("turn-started", json!({})));
        a.note("s", &ev("turn-finished", json!({"stopped_by": "stop"})));
        let snap = a.get("s").unwrap();
        assert_eq!(snap.state, "idle");
        assert_eq!(snap.outcome, "stop");
        assert!(snap.step.is_empty());

        a.note("s", &ev("turn-started", json!({})));
        a.note("s", &ev("tool-call", json!({"name": "ask_user"})));
        a.note("s", &ev("turn-finished", json!({"stopped_by": "asked"})));
        assert_eq!(a.get("s").unwrap().state, "waiting");

        a.note("s", &ev("turn-started", json!({})));
        a.note("s", &ev("turn-finished", json!({"stopped_by": "llm-error"})));
        let snap = a.get("s").unwrap();
        assert_eq!(snap.state, "failed");
        assert_eq!(snap.outcome, "llm-error");

        // A new user message resets the turn's tallies.
        a.note("s", &ev("user", json!({})));
        let snap = a.get("s").unwrap();
        assert_eq!(snap.state, "working");
        assert!(snap.outcome.is_empty());
        assert_eq!(snap.steps, 0);
    }

    #[test]
    fn children_are_counted_but_do_not_steer_the_parent() {
        let a = Activity::new();
        a.note("s", &ev("turn-started", json!({})));
        a.note("s", &ev("tool-call", json!({"name": "spawn_agent"})));
        a.note("s", &ev("turn-started", json!({"agent": "k1", "agent_label": "research"})));
        a.note("s", &ev("tool-call", json!({"agent": "k1", "name": "web-search"})));
        let snap = a.get("s").unwrap();
        assert_eq!(snap.agents, 1);
        assert_eq!(snap.step, "spawn_agent", "the child's tool is not the parent's step");
        assert_eq!(snap.steps, 1);
        a.note("s", &ev("turn-finished", json!({"agent": "k1", "stopped_by": "stop"})));
        assert_eq!(a.get("s").unwrap().agents, 0);
        assert_eq!(a.get("s").unwrap().state, "working", "the parent is still mid-turn");
    }

    #[test]
    fn non_event_frames_are_ignored() {
        let a = Activity::new();
        a.note("s", r#"{"type":"terminal","session":"s","data":"kind of"}"#);
        a.note("s", "not json");
        assert!(a.get("s").is_none());
    }

    #[test]
    fn the_wire_frame_names_the_session() {
        let change = Change {
            session_id: "abc".into(),
            snapshot: Snapshot { state: "working", step: "thinking".into(), ..Snapshot::default() },
        };
        let v: Value = serde_json::from_str(&Activity::frame(&change)).unwrap();
        assert_eq!(v["type"], "activity");
        assert_eq!(v["session"], "abc");
        assert_eq!(v["state"], "working");
        assert_eq!(v["step"], "thinking");
    }
}
