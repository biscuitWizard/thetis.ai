//! Session actors.
//!
//! One tokio task per active session, owning the turn lifecycle. This is what
//! makes concurrent input safe: a message arriving while a turn is running
//! becomes a *nudge* delivered into the running turn rather than a second turn
//! racing the first. Event ordering is therefore strictly serial per session,
//! even with several browser tabs attached.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;

use crate::bindings::types::{Attachment, InboxItem, SessionEvent, UserMsg};
use crate::grip::Grip;

pub enum SessionMsg {
    User(UserMsg),
    /// Continue a turn that was cut short, with no new user input. The agent is
    /// stateless between turns, so carrying on simply means running one again:
    /// it rebuilds its context from the log, which now records the interruption.
    Resume,
    Cancel,
}

struct Handle {
    tx: mpsc::UnboundedSender<SessionMsg>,
    inbox: Arc<Mutex<VecDeque<InboxItem>>>,
    /// Raised by `cancel`, watched by the running turn's wasm budget.
    ///
    /// The inbox message below is the *graceful* path: the agent sees it at
    /// its next `poll-inbox` checkpoint and stops tidily. But a guest inside a
    /// long tool call — a cargo build, a streaming completion — does not reach
    /// a checkpoint for minutes, and the Stop button was simply inert for the
    /// whole of it. This flag is checked by the epoch deadline callback, which
    /// fires while the guest is executing, so the turn ends either way.
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
pub struct SessionActors {
    handles: RwLock<HashMap<String, Handle>>,
}

impl SessionActors {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends a user message to a session, spawning its actor on first use.
    pub fn submit(
        &self,
        grip: &Arc<Grip>,
        session_id: &str,
        message: String,
        attachments: Vec<Attachment>,
    ) {
        let tx = self.ensure(grip, session_id);
        let _ = tx.send(SessionMsg::User(UserMsg {
            text: message,
            attachments,
        }));
    }

    /// Picks up a turn that was interrupted, spawning the actor if needed.
    pub fn resume(&self, grip: &Arc<Grip>, session_id: &str) {
        let tx = self.ensure(grip, session_id);
        let _ = tx.send(SessionMsg::Resume);
    }

    pub fn cancel(&self, session_id: &str) {
        if let Ok(handles) = self.handles.read() {
            if let Some(h) = handles.get(session_id) {
                h.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = h.tx.send(SessionMsg::Cancel);
            }
        }
    }

    /// The flag a running turn's budget watches, cleared for a fresh turn.
    ///
    /// Cleared rather than merely read: a cancel that arrived between turns
    /// must not kill the next one on sight.
    pub fn take_cancel_flag(
        &self,
        session_id: &str,
    ) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        let handles = self.handles.read().ok()?;
        let flag = handles.get(session_id)?.cancel.clone();
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        Some(flag)
    }

    /// Takes everything queued for the running turn. Called by the agent's
    /// `poll-inbox` import at its checkpoints.
    pub fn drain_inbox(&self, session_id: &str) -> Vec<InboxItem> {
        let Ok(handles) = self.handles.read() else {
            return Vec::new();
        };
        let Some(h) = handles.get(session_id) else {
            return Vec::new();
        };
        let Ok(mut inbox) = h.inbox.lock() else {
            return Vec::new();
        };
        inbox.drain(..).collect()
    }

    fn ensure(
        &self,
        grip: &Arc<Grip>,
        session_id: &str,
    ) -> mpsc::UnboundedSender<SessionMsg> {
        if let Ok(handles) = self.handles.read() {
            if let Some(h) = handles.get(session_id) {
                return h.tx.clone();
            }
        }

        let mut handles = match self.handles.write() {
            Ok(h) => h,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Another thread may have won the race between the two locks.
        if let Some(h) = handles.get(session_id) {
            return h.tx.clone();
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        handles.insert(
            session_id.to_string(),
            Handle {
                tx: tx.clone(),
                inbox: inbox.clone(),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );

        tokio::spawn(actor(grip.clone(), session_id.to_string(), rx, inbox));
        tx
    }
}

async fn actor(
    grip: Arc<Grip>,
    session_id: String,
    mut rx: mpsc::UnboundedReceiver<SessionMsg>,
    inbox: Arc<Mutex<VecDeque<InboxItem>>>,
) {
    // Set when a turn ends with unconsumed nudges: those are user input that
    // never reached the model, so they start a follow-up turn instead of being
    // silently dropped.
    let mut start_immediately = false;

    loop {
        if !start_immediately {
            match rx.recv().await {
                None => return, // registry dropped; session is going away
                Some(SessionMsg::Cancel) => continue, // nothing running to cancel
                // Nothing to append: the log already holds the user message
                // this turn is answering.
                Some(SessionMsg::Resume) => {
                    tracing::info!(session = %session_id, "resuming an interrupted turn");
                }
                Some(SessionMsg::User(msg)) => {
                    if let Err(e) = grip
                        .append_event(&session_id, SessionEvent::UserMessage(msg))
                        .await
                    {
                        tracing::error!(session = %session_id, error = %e, "failed to log user message");
                        continue;
                    }
                }
            }
        }
        start_immediately = false;

        if let Err(e) = grip.append_event(&session_id, SessionEvent::TurnStarted).await {
            tracing::error!(session = %session_id, error = %e, "failed to log turn start");
        }

        // Held across the whole select loop: a worker running a turn must
        // never look idle to the reaper, however long the model thinks.
        let _running = grip.begin_turn();
        let turn = grip.run_turn(&session_id);
        tokio::pin!(turn);

        let outcome = loop {
            tokio::select! {
                result = &mut turn => break result,
                incoming = rx.recv() => match incoming {
                    // Input during a turn steers the turn in flight.
                    // A nudge is text only: an image mid-turn would need the
                    // model to re-read the whole message, so those start a new turn.
                    Some(SessionMsg::User(msg)) => {
                        let _ = grip
                            .append_event(&session_id, SessionEvent::Nudge(msg.text.clone()))
                            .await;
                        push(&inbox, InboxItem::Nudge(msg.text));
                    }
                    Some(SessionMsg::Cancel) => push(&inbox, InboxItem::Cancel),
                    // A turn is already running; there is nothing to resume.
                    Some(SessionMsg::Resume) => {}
                    None => {
                        // Senders are gone, but the turn still deserves to finish.
                        break (&mut turn).await;
                    }
                },
            }
        };

        // Terminal commands and host filesystem writes bypass the build
        // pipeline's checkpoints; this sweep makes sure a turn can never end
        // with work that exists only in the working tree. Before the
        // turn-finished event on purpose: anything reacting to "the turn is
        // over" may rely on the branch log being current.
        let _ = grip.commit_worktree("checkpoint: end of turn").await;

        match outcome {
            Ok(stats) => {
                let _ = grip.append_event(&session_id, SessionEvent::TurnFinished(stats)).await;
                // The turn made it to the end, so a later interruption starts
                // counting from zero again.
                let _ = grip.persist.clear_resume_attempts(&session_id).await;
            }
            Err(err) => {
                tracing::warn!(session = %session_id, error = %err, "turn failed");
                let _ = grip
                    .append_event(&session_id, SessionEvent::Incident(err.to_string()))
                    .await;
                // Every turn ends with exactly one terminator, success or not.
                // Anything waiting on "the turn is over" watches for this one
                // event — the Discord bridge's reply loop breaks on nothing
                // else — so a failed turn used to leave it listening forever
                // and the user got silence instead of the error. `stopped-by`
                // carries the reason in-band rather than widening the record.
                let _ = grip
                    .append_event(
                        &session_id,
                        SessionEvent::TurnFinished(crate::bindings::types::TurnStats {
                            iterations: 0,
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            cost_usd: 0.0,
                            tools_used: Vec::new(),
                            stopped_by: "error".to_string(),
                        }),
                    )
                    .await;

                // Only the agent's own misbehaviour counts against its breaker;
                // a missing API key is not a reason to roll back the agent.
                if err.is_trap() {
                    if let Some(action) = crate::watchdog::report_failure(
                        &grip,
                        &crate::aspect::Aspect::Agent,
                        err.message(),
                    )
                    .await
                    {
                        let _ = grip
                            .append_event(&session_id, SessionEvent::Incident(action))
                            .await;
                    }
                }
            }
        }

        // Anything the agent never picked up becomes the seed of the next turn.
        let leftovers = take_all(&inbox);
        if leftovers
            .iter()
            .any(|i| matches!(i, InboxItem::Nudge(_)))
        {
            start_immediately = true;
        }
    }
}

fn push(inbox: &Arc<Mutex<VecDeque<InboxItem>>>, item: InboxItem) {
    if let Ok(mut q) = inbox.lock() {
        q.push_back(item);
    }
}

fn take_all(inbox: &Arc<Mutex<VecDeque<InboxItem>>>) -> Vec<InboxItem> {
    match inbox.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}
