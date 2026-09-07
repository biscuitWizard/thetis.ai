//! Spawning, watching and waiting on sub-agents.
//!
//! [`crate::subagents`] owns the *registry* — who is whose child, and what
//! state each child is in. This module owns the *behaviour*: creating a child
//! session, briefing it, waiting for it under a predicate, and cancelling it.
//! The split keeps the registry pure and testable against a bare store, while
//! everything that needs a running system lives here.
//!
//! ## Where a child runs
//!
//! In the parent's worker process, always. [`crate::workers::routing_key`]
//! resolves any session to its root before choosing a worker, so a child shares
//! the parent's checkout, branch and build cache. That is what makes delegation
//! useful for code work: a child that edited files in a worktree of its own
//! would leave the parent unable to see, build or commit the result.
//!
//! It also means waiting can be cheap. Parent and child are in one process, so
//! a child settling can ring a bell the parent is already sleeping on, instead
//! of the parent polling the database across an IPC boundary.

use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bindings::types::SessionEvent;
use crate::grip::Grip;
use crate::subagents::{SubagentRow, SubagentState};

/// Woken whenever a sub-agent reaches a terminal state.
///
/// Worker-local and advisory: every wait also has a backstop poll, so a missed
/// notification costs latency rather than correctness. Without the bell a
/// parent would have to poll the registry, which on a worker is an IPC call to
/// the gateway — a parent waiting half an hour would make thousands of them.
#[derive(Clone, Default)]
pub struct SettleBell(Arc<tokio::sync::Notify>);

impl SettleBell {
    pub fn ring(&self) {
        self.0.notify_waiters();
    }

    pub async fn wait(&self) {
        self.0.notified().await;
    }
}

/// Backstop poll interval for a wait. The bell handles the normal case; this is
/// only what keeps a missed ring from becoming a hang.
const POLL: Duration = Duration::from_secs(2);

/// Prefix of the system note that records a spawn in the parent's log.
///
/// The rest of the note is the child's session id, a space, and its label. This
/// is a wire format between the host and the web gateway, which reads it to
/// know which child logs to replay when a page reloads — the parent's log is
/// the only thing the browser asks for, and a child's turns are not in it.
/// Changing the shape means changing `gateways/gateway-web/src/handlers.rs`
/// with it.
pub const SPAWN_NOTE: &str = "subagent:spawned ";

/// What a parent is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFor {
    /// Nothing but the clock. The honest way to say "let something else run".
    Time,
    /// Every live child of this parent has finished.
    AllChildren,
    /// These specific children have all finished.
    Children(Vec<String>),
    /// The first of the named children — or of all of them — to finish.
    AnyChild(Vec<String>),
    /// Return as soon as any child fails, so the parent can react instead of
    /// paying for the rest of the fan-out. Also returns when all succeed.
    ///
    /// This exists because of a specific, documented failure mode: an
    /// orchestrator that waits for a whole batch, then discovers the first
    /// worker misunderstood the brief, has paid for every other worker's run
    /// before learning the brief was wrong.
    FirstFailure(Vec<String>),
}

impl WaitFor {
    /// Parses the agent's `until` argument.
    pub fn parse(until: &str, children: Vec<String>) -> Result<Self> {
        Ok(match until {
            "time" | "duration" => WaitFor::Time,
            "all" | "all_children" => {
                if children.is_empty() {
                    WaitFor::AllChildren
                } else {
                    WaitFor::Children(children)
                }
            }
            "any" | "any_child" => WaitFor::AnyChild(children),
            "first_failure" => WaitFor::FirstFailure(children),
            other => bail!(
                "unknown wait predicate `{other}`. Use one of: time, all, any, first_failure."
            ),
        })
    }

    /// The children this predicate concerns, given everything the parent has.
    /// An empty selector means "all of them", which is what makes `wait(all)`
    /// work without the parent having to list ids it already told us about.
    fn selected<'a>(&self, all: &'a [SubagentRow]) -> Vec<&'a SubagentRow> {
        let filter = match self {
            WaitFor::Time => return Vec::new(),
            WaitFor::AllChildren => return all.iter().collect(),
            WaitFor::Children(ids) | WaitFor::AnyChild(ids) | WaitFor::FirstFailure(ids) => ids,
        };
        if filter.is_empty() {
            return all.iter().collect();
        }
        all.iter()
            .filter(|r| filter.contains(&r.child_id))
            .collect()
    }

    /// Whether the wait is over, and why.
    fn satisfied(&self, all: &[SubagentRow]) -> Option<&'static str> {
        let rows = self.selected(all);
        match self {
            WaitFor::Time => None,
            // A parent that asks to wait for children it does not have is
            // satisfied at once rather than blocked to the deadline: the
            // alternative is a turn that appears to hang on a typo.
            WaitFor::AllChildren | WaitFor::Children(_) => rows
                .iter()
                .all(|r| r.state.is_terminal())
                .then_some("all finished"),
            WaitFor::AnyChild(_) => {
                if rows.is_empty() {
                    return Some("no such sub-agents");
                }
                rows.iter()
                    .any(|r| r.state.is_terminal())
                    .then_some("one finished")
            }
            WaitFor::FirstFailure(_) => {
                if rows.is_empty() {
                    return Some("no such sub-agents");
                }
                if rows
                    .iter()
                    .any(|r| matches!(r.state, SubagentState::Failed | SubagentState::Cancelled))
                {
                    return Some("a sub-agent failed");
                }
                rows.iter()
                    .all(|r| r.state.is_terminal())
                    .then_some("all finished")
            }
        }
    }
}

/// Names the profile, model and mode a child will run under.
pub struct SpawnRequest {
    pub label: String,
    pub task: String,
    /// Profile id from `[[subagents.profiles]]`, or empty for a plain child.
    pub profile: String,
    /// Explicit model override. Wins over the profile's.
    pub model: String,
    /// Explicit mode override. Wins over the profile's.
    pub mode: String,
}

/// Creates a child session, briefs it, and starts its turn.
///
/// Returns as soon as the child's turn has been *started*, never when it
/// finishes: a spawn that blocked would make concurrency impossible and would
/// leave the parent unable to fan out at all. The parent chooses to block by
/// calling [`wait`] afterwards, which is also how a synchronous delegation is
/// expressed — spawn, then wait on that one child.
/// Starts a sub-agent under the authority of the turn that asked for it.
///
/// `ceiling` is the spawning turn's *effective* policy — what the speaker could
/// actually do at the moment they delegated, not what the conversation's owner
/// could do. Stamped on the child so a delegated turn can never be a way to
/// gain authority the delegator did not have: `owner_of_root` walks a child to
/// its root before resolving policy, which alone would have given the child the
/// root owner's permissions regardless of who was speaking.
pub async fn spawn(
    grip: &Arc<Grip>,
    parent_id: &str,
    req: SpawnRequest,
    ceiling: Option<crate::policy::EffectivePolicy>,
) -> Result<SubagentRow> {
    let cfg = &grip.cfg;
    if !cfg.subagents.enabled {
        bail!("delegation is switched off (subagents.enabled = false)");
    }
    if req.task.trim().is_empty() {
        bail!("a sub-agent needs a task. An empty brief produces an empty answer.");
    }

    // The depth guard, checked here as well as in the registry. The registry's
    // check is the one that cannot be bypassed; this one exists so the refusal
    // arrives before a session has been created and orphaned.
    if grip
        .persist
        .get_subagent(parent_id)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        bail!(
            "a sub-agent cannot spawn sub-agents. Do this work yourself, or \
             finish and let your parent delegate it."
        );
    }

    // Resolve the profile before anything is created, so a bad profile name
    // costs nothing.
    let profile = if req.profile.is_empty() {
        None
    } else {
        match cfg.subagents.profile(&req.profile) {
            Some(p) => Some(p),
            None => bail!(
                "unknown agent profile `{}`. Configured profiles: {}",
                req.profile,
                if cfg.subagents.profiles.is_empty() {
                    "none".to_string()
                } else {
                    cfg.subagents
                        .profiles
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        }
    };

    let mode = first_non_empty(&[
        &req.mode,
        profile.map(|p| p.mode.as_str()).unwrap_or(""),
        &cfg.subagents.default_mode,
    ]);
    if cfg.mode(&mode).is_none() {
        bail!("unknown mode `{mode}` for a sub-agent");
    }
    let model = first_non_empty(&[&req.model, profile.map(|p| p.model.as_str()).unwrap_or("")]);
    if !model.is_empty() && !cfg.models.iter().any(|m| m.id == model) {
        bail!(
            "unknown model `{model}`. Configured models: {}",
            cfg.models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let label = if req.label.trim().is_empty() {
        req.profile.clone()
    } else {
        req.label.trim().to_string()
    };
    let label = if label.is_empty() {
        "sub-agent".to_string()
    } else {
        label
    };

    let owner = grip
        .persist
        .owner_of_root(parent_id)
        .await?
        .unwrap_or_else(|| "local".into());
    let child = grip
        .persist
        .create_session(Some(label.clone()), &mode, &owner)
        .await
        .context("creating the sub-agent's session")?;

    // Before the child can run, derive the ceiling it inherits from this turn
    // and from its parent conversation. It is handed to the trusted gateway as
    // part of registration so a worker never needs permission to set ceilings.
    let child_ceiling = if let Some(mut ceiling) = ceiling {
        if let Ok(Some(parent_ceiling)) = grip.persist.ceiling_of(parent_id).await {
            ceiling = ceiling.intersect(&parent_ceiling);
        }
        Some(ceiling)
    } else {
        None
    };

    // Registration and ceiling stamping happen together on the gateway. That
    // makes the child recognizable for routing before it can run, without any
    // interval in which an unstamped child could start.
    let row = match grip
        .persist
        .register_subagent(
            parent_id,
            &child.id,
            &label,
            req.task.trim(),
            profile.map(|p| p.id.as_str()).unwrap_or(""),
            &model,
            &mode,
            cfg.subagents.max_children,
            child_ceiling.as_ref(),
        )
        .await
    {
        Ok(row) => row,
        Err(e) => {
            // The cap, depth guard, or ceiling stamp refused. Leave no useful
            // unregistered session in the sidebar.
            let _ = grip.persist.archive_session(&child.id, true).await;
            return Err(e).context("registering the sub-agent and stamping its ceiling");
        }
    };

    if !model.is_empty() {
        let _ = grip.persist.set_model(&child.id, &model).await;
    }

    // Primed before the child's first event can be rendered. Without this the
    // render loop would look the tag up over IPC while the child's opening
    // frames were already in flight, and the first few would arrive untagged —
    // which the UI would draw as the parent talking to itself.
    remember_tag(
        &child.id,
        Some(ChildTag {
            child_id: row.child_id.clone(),
            parent_id: row.parent_id.clone(),
            root_id: row.root_id.clone(),
            label: row.label.clone(),
        }),
    );

    // The brief. Everything the delegation research says a worker needs, in
    // one message: who it is, what to do, what to hand back, and that nobody
    // will answer a follow-up question. The last part matters more than it
    // looks — a child that stops to ask something waits forever, because its
    // only correspondent is a parent that is itself mid-turn.
    let mut brief = String::new();
    if let Some(p) = profile {
        if !p.prompt.is_empty() {
            brief.push_str(&p.prompt);
            brief.push_str("\n\n");
        }
    }
    brief.push_str("You are a sub-agent, delegated one task by another agent.\n\n");
    brief.push_str("## Your task\n\n");
    brief.push_str(req.task.trim());
    brief.push_str(
        "\n\n## How to finish\n\n\
         Work until the task is done, then end your turn with your findings as a \
         plain answer. That final message is the only thing your parent receives — \
         it does not see your tool calls, your intermediate steps or your reasoning, \
         so anything it needs must be in the answer itself. Include what you \
         concluded, what you changed if you changed anything, and anything you could \
         not resolve.\n\n\
         Nobody will reply to you. You cannot ask questions and you cannot delegate \
         further. If the task is ambiguous, state the reading you chose and carry on.\n",
    );

    // No speaker: the brief comes from the system, so the child's first turn
    // resolves from its own ceiling and owner rather than from a person.
    grip.submit(&child.id, brief, Vec::new(), None).await?;

    // In the parent's transcript, so the delegation is visible in the log and
    // not only in a tool result the UI has to interpret.
    //
    // The format is load-bearing rather than decorative: it is the *only*
    // record in the parent's own log that names a child, and a child's turns
    // live in the child's log. On a reload the browser replays the parent's
    // events and nothing else, so without a machine-readable trail here every
    // sub-agent block would vanish on refresh and reappear only if the child
    // happened to still be running. The gateway parses this note to find the
    // logs it must also replay. See `SPAWN_NOTE`.
    let _ = grip
        .append_event(
            parent_id,
            SessionEvent::SystemNote(format!("{SPAWN_NOTE}{} {label}", child.id)),
        )
        .await;

    Ok(row)
}

/// The outcome of a wait.
pub struct WaitOutcome {
    /// Why the wait ended: a predicate description, or "timeout".
    pub reason: String,
    /// Every child of the parent, as it stands now.
    pub children: Vec<SubagentRow>,
    /// Whether the deadline, rather than the predicate, ended it.
    pub timed_out: bool,
}

/// Blocks until a predicate holds, the deadline passes, or the user stops the
/// parent's turn.
///
/// The deadline is not optional and is capped by configuration. A wait that
/// could run forever is indistinguishable from a hung turn, both to the user
/// watching the spinner and to the idle reaper deciding whether this worker is
/// still doing anything.
pub async fn wait(
    grip: &Arc<Grip>,
    parent_id: &str,
    predicate: &WaitFor,
    timeout: Duration,
) -> Result<WaitOutcome> {
    let cap = Duration::from_secs(grip.cfg.subagents.max_wait_secs);
    let timeout = timeout.min(cap);
    let deadline = Instant::now() + timeout;
    let cancel = grip.cancel_flag(parent_id);
    let bell = grip.settle_bell.clone();

    loop {
        let children = grip
            .persist
            .subagents_of(parent_id)
            .await
            .unwrap_or_default();
        if let Some(reason) = predicate.satisfied(&children) {
            return Ok(WaitOutcome {
                reason: reason.to_string(),
                children,
                timed_out: false,
            });
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(WaitOutcome {
                reason: "timeout".to_string(),
                children,
                timed_out: true,
            });
        }
        // A pure time wait has nothing to be woken for, so it sleeps out its
        // remaining span in one go rather than waking every couple of seconds.
        let slice = if matches!(predicate, WaitFor::Time) {
            deadline - now
        } else {
            POLL.min(deadline - now)
        };

        // Racing the stop signal is what makes the button work here. A wait
        // parked in a plain sleep would hold the turn open for its whole
        // duration after the user asked for it to end.
        match &cancel {
            Some(flag) => {
                tokio::select! {
                    _ = flag.cancelled() => bail!("the wait was stopped"),
                    _ = bell.wait() => {}
                    _ = tokio::time::sleep(slice) => {}
                }
            }
            None => {
                tokio::select! {
                    _ = bell.wait() => {}
                    _ = tokio::time::sleep(slice) => {}
                }
            }
        }
    }
}

/// Stops a sub-agent and records it as cancelled.
///
/// Marks the registry first, then signals the turn. That order is deliberate:
/// the mark is what a parent's `wait` observes, and doing it second would leave
/// a window where the turn is already unwinding but every watcher still sees a
/// child that is running.
pub async fn cancel_child(
    grip: &Arc<Grip>,
    parent_id: &str,
    child_id: &str,
) -> Result<SubagentRow> {
    let row = grip
        .persist
        .get_subagent(child_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("`{child_id}` is not a sub-agent"))?;
    if row.parent_id != parent_id {
        bail!("`{child_id}` is not one of this session's sub-agents");
    }
    let row = grip.persist.cancel_subagent(child_id).await?;
    grip.cancel(child_id).await;
    grip.settle_bell.ring();
    Ok(row)
}

/// Stops every live child of a session. Called when the parent's own turn is
/// cancelled, so a stop does not leave orphans burning tokens behind a
/// conversation the user has walked away from.
pub async fn cancel_all_children(grip: &Arc<Grip>, parent_id: &str) {
    let children = grip
        .persist
        .subagents_of(parent_id)
        .await
        .unwrap_or_default();
    for row in children.into_iter().filter(|r| !r.state.is_terminal()) {
        let _ = grip.persist.cancel_subagent(&row.child_id).await;
        grip.cancel(&row.child_id).await;
    }
    grip.settle_bell.ring();
}

/// A child's answer: the last thing it said with words in it.
///
/// Reading the log rather than trusting the turn's return value is the same
/// choice the spend ledger makes, and for the same reason — the log is
/// append-only and complete, while a turn that was interrupted or resumed
/// returns a partial view of itself.
pub async fn final_answer(grip: &Arc<Grip>, child_id: &str) -> String {
    let events = grip.persist.events(child_id, 0).await.unwrap_or_default();
    events
        .iter()
        .rev()
        .find_map(|r| match &r.event {
            SessionEvent::AssistantMessage(m) if !m.content.trim().is_empty() => {
                Some(m.content.trim().to_string())
            }
            _ => None,
        })
        .unwrap_or_default()
}

// --- frame tagging -----------------------------------------------------------

/// What the UI needs to draw a child's frame inside its parent's transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildTag {
    pub child_id: String,
    pub parent_id: String,
    pub root_id: String,
    pub label: String,
}

/// Session id -> whether it is a child, and if so its tag.
///
/// Every rendered event asks this question, and on a worker the authoritative
/// answer is an IPC round trip to the gateway's database. A busy turn emits
/// hundreds of events a second, so the answer is cached — and it is safe to
/// cache permanently, because a session's parentage is decided when it is
/// created and never changes afterwards. `None` means "an ordinary
/// conversation", which is also worth remembering: it is the common case and
/// the one that would otherwise pay for a lookup on every single frame.
static TAGS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Option<ChildTag>>>,
> = std::sync::OnceLock::new();

fn tags() -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<ChildTag>>> {
    TAGS.get_or_init(Default::default)
}

fn remember_tag(session_id: &str, tag: Option<ChildTag>) {
    if let Ok(mut map) = tags().lock() {
        // A worker lives for one conversation, so this map is bounded by that
        // conversation's fan-out. The clear is paranoia against a long-lived
        // gateway process rendering for many conversations.
        if map.len() > 4096 {
            map.clear();
        }
        map.insert(session_id.to_string(), tag);
    }
}

fn cached_tag(session_id: &str) -> Option<Option<ChildTag>> {
    tags().lock().ok()?.get(session_id).cloned()
}

/// The tag for a session, or `None` if it is a top-level conversation.
pub async fn frame_tag(grip: &Arc<Grip>, session_id: &str) -> Option<ChildTag> {
    if let Some(hit) = cached_tag(session_id) {
        return hit;
    }
    let tag = grip
        .persist
        .get_subagent(session_id)
        .await
        .ok()
        .flatten()
        .map(|row| ChildTag {
            child_id: row.child_id,
            parent_id: row.parent_id,
            root_id: row.root_id,
            label: row.label,
        });
    remember_tag(session_id, tag.clone());
    tag
}

/// Truncates a child's answer to what the parent's context should carry.
pub fn clamp_result(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n[the sub-agent's answer was {} bytes and is truncated here; \
         open its transcript for the rest]",
        &text[..cut],
        text.len()
    )
}

fn first_non_empty(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|c| !c.trim().is_empty())
        .map(|c| c.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: SubagentState) -> SubagentRow {
        SubagentRow {
            child_id: id.to_string(),
            parent_id: "p".into(),
            root_id: "p".into(),
            label: id.to_string(),
            task: "t".into(),
            agent_aspect: String::new(),
            model: String::new(),
            mode: "agent".into(),
            state,
            created_ms: 0,
            finished_ms: 0,
            result: String::new(),
            detail: String::new(),
            cost_usd: 0.0,
        }
    }

    #[test]
    fn all_waits_for_every_child() {
        let p = WaitFor::AllChildren;
        let mut rows = vec![
            row("a", SubagentState::Done),
            row("b", SubagentState::Running),
        ];
        assert!(p.satisfied(&rows).is_none());
        rows[1].state = SubagentState::Failed;
        assert_eq!(p.satisfied(&rows), Some("all finished"));
    }

    #[test]
    fn any_returns_on_the_first_one() {
        let p = WaitFor::AnyChild(Vec::new());
        let rows = vec![
            row("a", SubagentState::Running),
            row("b", SubagentState::Done),
        ];
        assert_eq!(p.satisfied(&rows), Some("one finished"));
    }

    #[test]
    fn a_named_wait_ignores_other_children() {
        let p = WaitFor::Children(vec!["a".into()]);
        let rows = vec![
            row("a", SubagentState::Done),
            row("b", SubagentState::Running),
        ];
        assert_eq!(p.satisfied(&rows), Some("all finished"));
    }

    #[test]
    fn first_failure_short_circuits_a_running_batch() {
        let p = WaitFor::FirstFailure(Vec::new());
        let rows = vec![
            row("a", SubagentState::Failed),
            row("b", SubagentState::Running),
        ];
        assert_eq!(p.satisfied(&rows), Some("a sub-agent failed"));
    }

    #[test]
    fn first_failure_still_waits_when_everything_is_going_well() {
        let p = WaitFor::FirstFailure(Vec::new());
        let rows = vec![
            row("a", SubagentState::Done),
            row("b", SubagentState::Running),
        ];
        assert!(p.satisfied(&rows).is_none());
    }

    /// A wait for children that do not exist must end, not hang. A typo in an
    /// id is otherwise indistinguishable from a child that never finishes.
    #[test]
    fn waiting_on_nothing_returns_rather_than_hanging() {
        let rows = vec![row("a", SubagentState::Running)];
        assert_eq!(
            WaitFor::AnyChild(vec!["ghost".into()]).satisfied(&rows),
            Some("no such sub-agents")
        );
        assert_eq!(
            WaitFor::Children(vec!["ghost".into()]).satisfied(&rows),
            Some("all finished")
        );
    }

    #[test]
    fn a_time_wait_is_never_satisfied_early() {
        let rows = vec![row("a", SubagentState::Done)];
        assert!(WaitFor::Time.satisfied(&rows).is_none());
    }

    #[test]
    fn predicates_parse_from_what_the_model_writes() {
        assert_eq!(WaitFor::parse("time", vec![]).unwrap(), WaitFor::Time);
        assert_eq!(WaitFor::parse("all", vec![]).unwrap(), WaitFor::AllChildren);
        assert_eq!(
            WaitFor::parse("all", vec!["x".into()]).unwrap(),
            WaitFor::Children(vec!["x".into()])
        );
        assert!(WaitFor::parse("whenever", vec![]).is_err());
    }

    #[test]
    fn a_clamped_result_says_that_it_was_clamped() {
        let long = "é".repeat(200);
        let out = clamp_result(&long, 51);
        assert!(out.contains("truncated here"));
        // Never splits a character, even when the limit lands mid-sequence.
        assert!(out.starts_with(&"é".repeat(25)));
    }

    #[test]
    fn an_override_beats_a_profile() {
        assert_eq!(first_non_empty(&["", "  ", "b", "c"]), "b");
        assert_eq!(first_non_empty(&["", ""]), "");
    }
}
