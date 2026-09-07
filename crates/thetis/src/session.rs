//! Session actors.
//!
//! One tokio task per active session, owning the turn lifecycle. This is what
//! makes concurrent input safe: a message arriving while a turn is running
//! becomes a *nudge* delivered into the running turn rather than a second turn
//! racing the first. Event ordering is therefore strictly serial per session,
//! even with several browser tabs attached.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;

use crate::bindings::types::{Attachment, InboxItem, SessionEvent, TurnStats, UserMsg};
use crate::grip::Grip;

pub enum SessionMsg {
    User(UserMsg),
    /// Continue a turn that was cut short, with no new user input. The agent is
    /// stateless between turns, so carrying on simply means running one again:
    /// it rebuilds its context from the log, which now records the interruption.
    Resume,
}

/// A session's stop signal.
///
/// Cancellation cannot be a message on the actor's channel and nothing else.
/// The thing a stop has to beat is a guest that is *already* inside a blocking
/// host call — a terminal command, a model stream — and a queued message is not
/// read until that call returns, which is exactly the wait the user is trying
/// to cut short. So the state lives here instead: set synchronously by whoever
/// handles the click, readable from any host import without a lock, and with a
/// [`tokio::sync::Notify`] so a pending import can wake on it rather than run
/// to its own deadline.
///
/// Staleness is handled by generation rather than by clearing, which removes
/// the race a clear would have with a stop arriving beside it. `turn` counts
/// turns; `stop_at` records the turn a stop was raised for. The flag reads as
/// raised only while the two agree, so:
///
/// * a stop during turn 4 stops turn 4;
/// * the same stop is stale the moment turn 5 begins;
/// * a stop while nothing is running affects nothing, because the next turn
///   bumps `turn` past it.
#[derive(Default)]
pub struct CancelFlag {
    turn: AtomicU64,
    stop_at: AtomicU64,
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    /// Opens a new turn, making any earlier stop stale. Returns its number.
    pub fn begin_turn(&self) -> u64 {
        self.turn.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Raises a stop against whatever turn is current.
    ///
    /// A no-op before the first turn: `stop_at` of zero never matches, so a
    /// stop that arrives with nothing to stop cannot ambush the next turn.
    pub fn raise(&self) {
        let turn = self.turn.load(Ordering::SeqCst);
        if turn == 0 {
            return;
        }
        self.stop_at.store(turn, Ordering::SeqCst);
        // Wake every import currently waiting, not just one: a turn can have
        // several in flight, and all of them are now pointless.
        self.notify.notify_waiters();
    }

    /// Whether the turn running right now has been stopped.
    pub fn raised(&self) -> bool {
        let stop_at = self.stop_at.load(Ordering::SeqCst);
        stop_at != 0 && stop_at == self.turn.load(Ordering::SeqCst)
    }

    /// Resolves as soon as the current turn is stopped.
    ///
    /// Checks before waiting, so a stop raised between a caller's own check and
    /// this call is not missed.
    pub async fn cancelled(&self) {
        loop {
            // Registering interest before the check is what closes the gap: a
            // `raise` landing after this point wakes the notified future rather
            // than being lost between the check and the wait.
            let waiting = self.notify.notified();
            if self.raised() {
                return;
            }
            waiting.await;
        }
    }
}

struct Handle {
    tx: mpsc::UnboundedSender<SessionMsg>,
    inbox: Arc<Mutex<VecDeque<InboxItem>>>,
    cancel: Arc<CancelFlag>,
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

    /// Stops the turn running for this session, if there is one.
    ///
    /// Everything here is done synchronously, before returning, because the
    /// actor task may not be scheduled again until the guest's current host
    /// call returns — and that call is the wait being cut short. Routing the
    /// stop through the actor's channel is what used to make the button look
    /// broken: the message sat in the queue behind the very thing it was meant
    /// to interrupt.
    ///
    /// Reports whether a live session was found, so the caller can tell the
    /// user "stopping" from "nothing was running".
    pub fn cancel(&self, session_id: &str) -> bool {
        let Ok(handles) = self.handles.read() else {
            return false;
        };
        let Some(h) = handles.get(session_id) else {
            return false;
        };
        // The flag first: it is what a blocking import checks, and what makes
        // the stop stick even if the guest never polls its inbox again.
        h.cancel.raise();
        // The inbox item too, so a well-behaved guest stops at its next
        // checkpoint with a tidy "cancelled" rather than by trapping.
        push(&h.inbox, InboxItem::Cancel);
        true
    }

    /// The stop signal for a session, for host imports that want to wait on it.
    pub fn cancel_flag(&self, session_id: &str) -> Option<Arc<CancelFlag>> {
        self.handles.read().ok()?.get(session_id).map(|h| h.cancel.clone())
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
        let cancel = Arc::new(CancelFlag::default());
        handles.insert(
            session_id.to_string(),
            Handle {
                tx: tx.clone(),
                inbox: inbox.clone(),
                cancel: cancel.clone(),
            },
        );

        tokio::spawn(actor(
            grip.clone(),
            session_id.to_string(),
            rx,
            inbox,
            cancel,
        ));
        tx
    }
}

async fn actor(
    grip: Arc<Grip>,
    session_id: String,
    mut rx: mpsc::UnboundedReceiver<SessionMsg>,
    inbox: Arc<Mutex<VecDeque<InboxItem>>>,
    cancel: Arc<CancelFlag>,
) {
    // Set when a turn ends with unconsumed nudges: those are user input that
    // never reached the model, so they start a follow-up turn instead of being
    // silently dropped.
    let mut start_immediately = false;

    loop {
        if !start_immediately {
            match rx.recv().await {
                None => return, // registry dropped; session is going away
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

        // Anything a previous turn left unread would otherwise stop this one
        // before it says a word. Whatever is worth carrying forward was already
        // taken as `leftovers` at the end of that turn.
        let _ = take_all(&inbox);
        // Opens this turn's cancellation generation, making any earlier stop
        // stale. Must happen before the first host call the guest can make.
        cancel.begin_turn();

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

        // Whether the user stopped this turn. Read before the next turn can
        // bump the generation, and used below to tell a stop apart from a fault.
        let stopped = cancel.raised();

        match outcome {
            Ok(stats) => {
                let _ = grip.append_event(&session_id, SessionEvent::TurnFinished(stats)).await;
                // The turn made it to the end, so a later interruption starts
                // counting from zero again.
                let _ = grip.persist.clear_resume_attempts(&session_id).await;
            }
            // A stop is the user getting what they asked for, so it ends the
            // turn the way a normal one ends: `turn-finished`, not an incident.
            // This is what clears "working…" in the composer — an interrupted
            // guest usually comes back as a trap, and reporting that as an
            // incident made a successful stop read as a crash.
            Err(_) if stopped => {
                tracing::info!(session = %session_id, "turn stopped by the user");
                let _ = grip
                    .append_event(
                        &session_id,
                        SessionEvent::TurnFinished(TurnStats {
                            iterations: 0,
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            cost_usd: 0.0,
                            tools_used: Vec::new(),
                            stopped_by: "cancelled".to_string(),
                        }),
                    )
                    .await;
                // Nothing to pick up later: the user asked for this to end.
                let _ = grip.persist.clear_resume_attempts(&session_id).await;
                let _ = grip.persist.set_no_resume(&session_id, true).await;
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
        // Unless the user stopped it. A nudge and a stop can arrive together —
        // typing, then hitting stop — and starting a follow-up turn on the
        // nudge would restart the work that was just cancelled.
        if !stopped
            && leftovers
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_fresh_flag_is_not_raised() {
        let flag = CancelFlag::default();
        assert!(!flag.raised());
    }

    #[test]
    fn a_stop_during_a_turn_stops_that_turn() {
        let flag = CancelFlag::default();
        flag.begin_turn();
        assert!(!flag.raised(), "starting a turn must not stop it");
        flag.raise();
        assert!(flag.raised());
    }

    #[test]
    fn a_stop_with_nothing_running_does_not_ambush_the_next_turn() {
        // The stop button on a conversation that is not doing anything must not
        // arm a trap for whatever the user sends next.
        let flag = CancelFlag::default();
        flag.raise();
        assert!(!flag.raised(), "there was no turn to stop");
        flag.begin_turn();
        assert!(!flag.raised(), "the next turn must start clean");
    }

    #[test]
    fn a_stop_does_not_leak_into_the_following_turn() {
        let flag = CancelFlag::default();
        flag.begin_turn();
        flag.raise();
        assert!(flag.raised());

        // The turn ends and another begins: the old stop is spent.
        flag.begin_turn();
        assert!(
            !flag.raised(),
            "a stale stop would cancel every later turn instantly"
        );
    }

    #[test]
    fn a_stop_stays_raised_for_the_rest_of_its_turn() {
        // Every host import and every guest checkpoint has to agree, however
        // many times they ask.
        let flag = CancelFlag::default();
        flag.begin_turn();
        flag.raise();
        for _ in 0..100 {
            assert!(flag.raised());
        }
    }

    #[test]
    fn repeated_stops_are_harmless() {
        let flag = CancelFlag::default();
        flag.begin_turn();
        flag.raise();
        flag.raise();
        flag.raise();
        assert!(flag.raised());
        // And the generation is untouched, so the next turn still runs.
        flag.begin_turn();
        assert!(!flag.raised());
    }

    #[test]
    fn turns_are_numbered_in_order() {
        let flag = CancelFlag::default();
        assert_eq!(flag.begin_turn(), 1);
        assert_eq!(flag.begin_turn(), 2);
        assert_eq!(flag.begin_turn(), 3);
    }

    #[tokio::test]
    async fn waiting_on_a_stop_that_already_happened_returns_at_once() {
        // The race that made the button unreliable: a blocking import checks
        // the flag, a stop lands, and the import then waits for a notification
        // that has already been sent. It must not block.
        let flag = CancelFlag::default();
        flag.begin_turn();
        flag.raise();

        tokio::time::timeout(Duration::from_secs(1), flag.cancelled())
            .await
            .expect("a stop already raised must not block a waiter");
    }

    #[tokio::test]
    async fn a_waiter_wakes_when_the_stop_arrives() {
        let flag = Arc::new(CancelFlag::default());
        flag.begin_turn();

        let waiting = tokio::spawn({
            let flag = flag.clone();
            async move { flag.cancelled().await }
        });

        // Let the waiter park before the stop is raised.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "nothing has been stopped yet");

        flag.raise();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the waiter must wake on a stop")
            .unwrap();
    }

    #[tokio::test]
    async fn every_waiter_wakes_not_just_one() {
        // A turn can have several imports in flight — a stream and a terminal
        // command. Waking only one would leave the others waiting out their
        // own deadlines.
        let flag = Arc::new(CancelFlag::default());
        flag.begin_turn();

        let waiters: Vec<_> = (0..5)
            .map(|_| {
                let flag = flag.clone();
                tokio::spawn(async move { flag.cancelled().await })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(50)).await;
        flag.raise();

        for w in waiters {
            tokio::time::timeout(Duration::from_secs(1), w)
                .await
                .expect("every waiter must wake")
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_waiter_ignores_a_stop_aimed_at_an_earlier_turn() {
        let flag = Arc::new(CancelFlag::default());
        flag.begin_turn();
        flag.raise();
        // Turn two: the stop above is now stale.
        flag.begin_turn();

        let waiting = tokio::spawn({
            let flag = flag.clone();
            async move { flag.cancelled().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiting.is_finished(),
            "a spent stop must not cancel the turn after it"
        );
        waiting.abort();
    }

    #[test]
    fn cancel_reports_whether_there_was_a_session_to_stop() {
        let actors = SessionActors::new();
        assert!(
            !actors.cancel("nobody"),
            "an unknown session cannot be stopped"
        );
        assert!(actors.cancel_flag("nobody").is_none());
    }
}
