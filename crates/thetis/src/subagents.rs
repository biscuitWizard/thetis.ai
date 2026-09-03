//! Sub-agents: sessions an agent spawns to work on its behalf.
//!
//! A sub-agent is a *child session*. It has its own event log, its own context
//! window and its own turn loop, but it shares the parent's worker process,
//! branch and checkout. That split is deliberate:
//!
//! - Isolating **context** is the whole point. A child explores a subproblem
//!   and hands back a summary, so the parent pays for the conclusion rather
//!   than for every step that led to it.
//! - Sharing the **checkout** is what makes delegation useful for code work. A
//!   child that edited files in a worktree of its own would leave the parent
//!   unable to see, build or commit the result.
//!
//! Parentage lives in its own redb table rather than as a field on
//! `SessionMeta`, because that record is shared with every WebAssembly guest
//! and widening it would break them at instantiation.
//!
//! ## Depth
//!
//! A sub-agent cannot spawn sub-agents. That is not a stylistic choice: an
//! agent that can delegate to a delegate can build an unbounded tree, and the
//! literature on multi-agent failures names unbounded fan-out as one of the
//! recurring ways such systems collapse. One level keeps the accounting, the
//! cancellation semantics and the UI all comprehensible, and the parent stays
//! identifiable as the thing responsible for the work.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};

/// Where a sub-agent is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    /// Spawned, its first turn not yet finished.
    Running,
    /// Finished a turn and produced a result.
    Done,
    /// Finished without producing anything usable, or faulted.
    Failed,
    /// Stopped by the parent or by the user.
    Cancelled,
}

impl SubagentState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, SubagentState::Running)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SubagentState::Running => "running",
            SubagentState::Done => "done",
            SubagentState::Failed => "failed",
            SubagentState::Cancelled => "cancelled",
        }
    }
}

/// One spawned sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRow {
    /// The child's own session id.
    pub child_id: String,
    /// The session that spawned it.
    pub parent_id: String,
    /// The top-level conversation this belongs to. With one level of nesting
    /// this equals `parent_id`, but it is stored rather than derived so that
    /// worker routing and frame delivery do not have to walk the chain.
    pub root_id: String,
    /// A short human label, shown in the parent's transcript.
    pub label: String,
    /// What the parent asked for, kept so the UI and a later reader can see
    /// what this child was for without opening its log.
    pub task: String,
    /// The agent aspect running it — an alternate agent build, or empty for the
    /// same one the parent runs.
    #[serde(default)]
    pub agent_aspect: String,
    /// Model override, empty for the grip default.
    #[serde(default)]
    pub model: String,
    /// The mode the child runs in, e.g. "agent" or "plan".
    pub mode: String,
    pub state: SubagentState,
    pub created_ms: u64,
    #[serde(default)]
    pub finished_ms: u64,
    /// The child's final answer, filled in when it finishes.
    #[serde(default)]
    pub result: String,
    /// Why it failed, when it did.
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub cost_usd: f64,
}

impl SubagentRow {
    /// A compact JSON view for a tool result or a wire frame.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.child_id,
            "parent": self.parent_id,
            "root": self.root_id,
            "label": self.label,
            "task": self.task,
            "agent": self.agent_aspect,
            "model": self.model,
            "mode": self.mode,
            "state": self.state.as_str(),
            "created_ms": self.created_ms,
            "finished_ms": self.finished_ms,
            "result": self.result,
            "detail": self.detail,
            "cost_usd": self.cost_usd,
        })
    }
}

/// Reads and writes the sub-agent registry.
///
/// Gateway-side only, because the registry lives in the database and only the
/// gateway holds it open. A worker reaches these through `Persist`.
///
/// Borrows the store rather than holding an `Arc`, so it is equally cheap to
/// build from the gateway's `Arc<Store>` and from the `&Store` the IPC store
/// server is handed.
#[derive(Clone, Copy)]
pub struct Subagents<'a> {
    store: &'a Store,
}

impl<'a> Subagents<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    pub fn get(&self, child_id: &str) -> Result<Option<SubagentRow>> {
        self.store.get_subagent(child_id)
    }

    /// Whether this session is itself a sub-agent. The depth guard.
    pub fn is_child(&self, session_id: &str) -> bool {
        matches!(self.store.get_subagent(session_id), Ok(Some(_)))
    }

    /// The top-level conversation a session belongs to — itself, if it is one.
    ///
    /// This is the worker routing key: a child must run in its parent's worker
    /// so that both see the same checkout.
    pub fn root_of(&self, session_id: &str) -> String {
        match self.store.get_subagent(session_id) {
            Ok(Some(row)) => row.root_id,
            _ => session_id.to_string(),
        }
    }

    pub fn children_of(&self, parent_id: &str) -> Result<Vec<SubagentRow>> {
        self.store.subagents_of(parent_id)
    }

    pub fn under(&self, root_id: &str) -> Result<Vec<SubagentRow>> {
        self.store.subagents_under(root_id)
    }

    /// Registers a freshly created child session.
    ///
    /// Refuses when the parent is itself a child, and when the parent already
    /// has `max_children` live children. Both refusals are the caller's to
    /// report to the model.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        parent_id: &str,
        child_id: &str,
        label: &str,
        task: &str,
        agent_aspect: &str,
        model: &str,
        mode: &str,
        max_children: usize,
    ) -> Result<SubagentRow> {
        if self.is_child(parent_id) {
            bail!(
                "a sub-agent cannot spawn sub-agents. Do the work in this session, \
                 or report back and let the parent delegate."
            );
        }
        let existing = self.store.subagents_of(parent_id)?;
        let live = existing
            .iter()
            .filter(|r| !r.state.is_terminal())
            .count();
        if max_children > 0 && live >= max_children {
            bail!(
                "this session already has {live} sub-agents running, which is the \
                 configured limit ({max_children}). Wait for one to finish first."
            );
        }
        let row = SubagentRow {
            child_id: child_id.to_string(),
            parent_id: parent_id.to_string(),
            // One level of nesting, so the root is the parent. Stored anyway,
            // so routing never has to walk a chain.
            root_id: parent_id.to_string(),
            label: label.to_string(),
            task: task.to_string(),
            agent_aspect: agent_aspect.to_string(),
            model: model.to_string(),
            mode: mode.to_string(),
            state: SubagentState::Running,
            created_ms: now_ms(),
            finished_ms: 0,
            result: String::new(),
            detail: String::new(),
            cost_usd: 0.0,
        };
        self.store.put_subagent(&row)?;
        Ok(row)
    }

    /// Records that a child's turn ended.
    ///
    /// A child that finishes with nothing to say is recorded as **failed**, not
    /// as an empty success. Reporting silence as completion is how a parent
    /// comes to build on a conclusion that was never reached — the failure mode
    /// the multi-agent literature calls premature termination — so the
    /// distinction is made here, once, rather than left to each caller.
    pub fn settle(
        &self,
        child_id: &str,
        result: &str,
        cost_usd: f64,
        stopped_by: &str,
    ) -> Result<SubagentRow> {
        let trimmed = result.trim();
        let state = match stopped_by {
            "cancelled" => SubagentState::Cancelled,
            // `asked` means the child called `ask_user`, which for a sub-agent
            // is a dead end: its only correspondent is a parent that is itself
            // mid-turn and cannot answer. Treating that as success would hand
            // the parent a question dressed up as a finding.
            "error" | "llm-error" | "asked" | "max-iterations" => SubagentState::Failed,
            _ if trimmed.is_empty() => SubagentState::Failed,
            _ => SubagentState::Done,
        };
        let detail = match (state, stopped_by) {
            (SubagentState::Failed, "asked") => {
                "the sub-agent asked a question instead of finishing; nobody could answer it"
                    .to_string()
            }
            (SubagentState::Failed, "max-iterations") => {
                "the sub-agent hit its iteration ceiling before finishing".to_string()
            }
            (SubagentState::Failed, _) if trimmed.is_empty() => {
                format!("the sub-agent's turn ended ({stopped_by}) without a final answer")
            }
            (SubagentState::Failed, _) => format!("the sub-agent's turn ended: {stopped_by}"),
            _ => String::new(),
        };
        self.store.update_subagent(child_id, |row| {
            row.state = state;
            row.result = trimmed.to_string();
            row.detail = detail;
            row.cost_usd = cost_usd;
            row.finished_ms = now_ms();
        })
    }

    /// Fails every child still recorded as running. Returns what it changed.
    ///
    /// Called once, at gateway startup, where "recorded as running" can only
    /// mean a child whose turn died with the process that was running it —
    /// nothing is in flight before the gateway is up. Without this the row
    /// stays `Running` forever: a sub-agent session is not a conversation, so
    /// `reconcile_interrupted_turns` never looks at it and never resumes it,
    /// and the parent is left waiting on a turn that no longer exists. A
    /// `wait until: "all"` on one of those cannot do anything but burn
    /// `max_wait_secs` and time out, every time it is called, for the life of
    /// the database.
    pub fn fail_orphans(&self, detail: &str) -> Result<Vec<SubagentRow>> {
        let mut settled = Vec::new();
        for row in self.store.all_subagents()? {
            if row.state.is_terminal() {
                continue;
            }
            // The row's own `cost_usd` is only written when a child settles
            // normally, so an orphan would otherwise be recorded as free. The
            // ledger has been counting all along; take what it says, so the
            // money a dead turn actually spent stays on the books.
            let spent = self.store.get_spend(&row.child_id).unwrap_or(0.0);
            let updated = self.store.update_subagent(&row.child_id, |row| {
                row.state = SubagentState::Failed;
                row.finished_ms = now_ms();
                if row.cost_usd == 0.0 {
                    row.cost_usd = spent;
                }
                if row.detail.is_empty() {
                    row.detail = detail.to_string();
                }
            })?;
            settled.push(updated);
        }
        Ok(settled)
    }

    /// Marks a child cancelled without waiting for its turn to unwind.
    pub fn mark_cancelled(&self, child_id: &str) -> Result<SubagentRow> {
        self.store.update_subagent(child_id, |row| {
            if !row.state.is_terminal() {
                row.state = SubagentState::Cancelled;
                row.finished_ms = now_ms();
                if row.detail.is_empty() {
                    row.detail = "stopped before it finished".to_string();
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!("thetis-sub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join("db.redb")).unwrap()
    }

    #[test]
    fn a_child_cannot_spawn_a_child() {
        let store = store();
        let subs = Subagents::new(&store);
        subs.register("parent", "kid", "k", "task", "", "", "agent", 8)
            .unwrap();
        let err = subs
            .register("kid", "grandkid", "g", "task", "", "", "agent", 8)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot spawn sub-agents"), "{err}");
    }

    #[test]
    fn fan_out_is_capped_by_live_children_only() {
        let store = store();
        let subs = Subagents::new(&store);
        for i in 0..2 {
            subs.register("p", &format!("c{i}"), "c", "t", "", "", "agent", 2)
                .unwrap();
        }
        assert!(subs.register("p", "c2", "c", "t", "", "", "agent", 2).is_err());
        // Settling one frees a slot: the cap is on concurrency, not on how many
        // a turn may delegate in total.
        subs.settle("c0", "an answer", 0.0, "stop").unwrap();
        assert!(subs.register("p", "c2", "c", "t", "", "", "agent", 2).is_ok());
    }

    // A child orphaned by a restart is not running and never will be again.
    // Left as `running` it is something a parent can wait on forever.
    #[test]
    fn a_startup_sweep_settles_children_nothing_is_running_any_more() {
        let store = store();
        let subs = Subagents::new(&store);
        subs.register("p", "alive", "a", "t", "", "", "agent", 8).unwrap();
        subs.register("p", "done", "d", "t", "", "", "agent", 8).unwrap();
        subs.settle("done", "an answer", 0.5, "stop").unwrap();

        let swept = subs.fail_orphans("orphaned by a restart").unwrap();
        assert_eq!(swept.len(), 1, "only the running one is an orphan");
        assert_eq!(swept[0].child_id, "alive");
        assert_eq!(swept[0].state, SubagentState::Failed);
        assert!(swept[0].detail.contains("orphaned"));

        // A child that had already finished keeps the answer it gave.
        let done = subs.get("done").unwrap().unwrap();
        assert_eq!(done.state, SubagentState::Done);
        assert_eq!(done.result, "an answer");
    }

    // A turn that died still spent what it spent. Recording the orphan as
    // free would quietly take it off the books.
    #[test]
    fn a_swept_orphan_keeps_the_money_its_turn_spent() {
        let store = store();
        let subs = Subagents::new(&store);
        subs.register("p", "c", "c", "t", "", "", "agent", 8).unwrap();
        store.add_spend("c", 4.25).unwrap();

        let swept = subs.fail_orphans("orphaned").unwrap();
        assert!((swept[0].cost_usd - 4.25).abs() < f64::EPSILON, "{:?}", swept[0]);
    }

    #[test]
    fn a_second_sweep_finds_nothing_left_to_do() {
        let store = store();
        let subs = Subagents::new(&store);
        subs.register("p", "c", "c", "t", "", "", "agent", 8).unwrap();
        assert_eq!(subs.fail_orphans("orphaned").unwrap().len(), 1);
        assert!(subs.fail_orphans("orphaned").unwrap().is_empty());
    }

    #[test]
    fn an_empty_result_is_a_failure_not_a_success() {
        let store = store();
        let subs = Subagents::new(&store);
        subs.register("p", "c", "c", "t", "", "", "agent", 8).unwrap();
        let row = subs.settle("c", "   ", 0.01, "stop").unwrap();
        assert_eq!(row.state, SubagentState::Failed);
        assert!(row.detail.contains("without a final answer"), "{}", row.detail);
    }

    #[test]
    fn root_of_walks_to_the_conversation() {
        let store = store();
        let subs = Subagents::new(&store);
        subs.register("conv", "kid", "k", "t", "", "", "agent", 8)
            .unwrap();
        assert_eq!(subs.root_of("kid"), "conv");
        assert_eq!(subs.root_of("conv"), "conv");
        assert!(subs.is_child("kid"));
        assert!(!subs.is_child("conv"));
    }

    #[test]
    fn children_are_hidden_from_the_session_list() {
        let store = store();
        let subs = Subagents::new(&store);
        let parent = store.create_session(Some("parent".into()), "agent").unwrap();
        let child = store.create_session(Some("child".into()), "agent").unwrap();
        subs.register(&parent.id, &child.id, "k", "t", "", "", "agent", 8)
            .unwrap();
        let listed = store.list_sessions(true).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, parent.id);
    }
}
