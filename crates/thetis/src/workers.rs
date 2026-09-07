//! The worker fleet, seen from the gateway.
//!
//! One worker per live conversation, each running against that conversation's
//! own git worktree. Workers are children of the gateway, not systemd units:
//! the control socket is inherited at spawn, a replacement can be a
//! *different* binary (a branch that rebuilt its own kernel), and
//! `PR_SET_PDEATHSIG` guarantees a dying gateway takes its workers with it —
//! no orphan can ever squat on a worktree or wedge a terminal.
//!
//! Materialization is lazy: a conversation gets its branch, worktree, and
//! worker at its first message. A worker that dies is respawned by the next
//! resume; one that keeps dying within its first seconds is left down, loudly,
//! because restarting harder is how failure loops start. Idle workers are
//! reaped after a quiet period — their branch state is all on disk and in the
//! gateway's database, so nothing is lost by stopping one.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::fd::IntoRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::branches::Branches;
use crate::grip::{Grip, RenderedFrame, Role};
use crate::ipc::{self, Peer};

/// The fd number a worker finds its gateway socket on.
pub const WORKER_SOCKET_FD: i32 = 3;

/// A worker that dies faster than this twice in a row stays down until the
/// user sends another message.
const MIN_UPTIME: Duration = Duration::from_secs(20);

/// How long a conversation sits quiet before its worker is stopped. Cheap to
/// come back: branch state is on disk, and artifacts load from the cache.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long `ensure` waits for a fresh worker to report ready. First
/// materialization may have to build every aspect from source.
const READY_TIMEOUT: Duration = Duration::from_secs(900);

/// Makes each kernel staging file unique, so two adoptions cannot share one.
static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many interrupted turns are resumed at once after a restart. Serial was
/// far too slow (each resume materializes a worker); unbounded would spawn a
/// worker per interrupted conversation simultaneously and thrash the machine.
const RESUME_CONCURRENCY: usize = 4;

/// Outer bound on one resume, so a single wedged branch cannot hold a resume
/// aspect indefinitely.
const RESUME_TIMEOUT: Duration = Duration::from_secs(READY_TIMEOUT.as_secs() + 30);

/// The gateway keeps a session's chosen starting revision here until the
/// first message materializes the branch.
pub const PENDING_BASE_KEY: &str = "__branch_base";

struct WorkerEntry {
    peer: Arc<Peer>,
    ready: tokio::sync::watch::Receiver<bool>,
    last_activity: std::sync::Mutex<Instant>,
    /// Set before any deliberate `shutdown`, so the supervisor can tell a
    /// requested stop from a crash. Without it, two admin "stop worker" clicks
    /// inside MIN_UPTIME counted as two fast deaths and threw away a perfectly
    /// good branch kernel with "it kept crashing at startup".
    stopping: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct DeathLedger {
    /// session -> consecutive fast deaths.
    fast_deaths: HashMap<String, u32>,
    last_start: HashMap<String, Instant>,
}

/// Where the gateway sends work bound for a conversation.
#[derive(Default)]
pub struct WorkerRouter {
    workers: RwLock<HashMap<String, Arc<WorkerEntry>>>,
    deaths: std::sync::Mutex<DeathLedger>,
    /// One lock *per session*, so two racing submits cannot spawn two workers
    /// for one conversation. Deliberately not a single fleet-wide lock: that
    /// one was held across `wait_ready`, so materializing one conversation
    /// (up to READY_TIMEOUT, and a cold branch really can take minutes to
    /// build every aspect) stopped every *other* conversation's first message,
    /// and any UI frame that reaches a worker, for the whole wait.
    materializing: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkerRouter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sessions with a live worker right now.
    pub async fn live_sessions(&self) -> Vec<String> {
        self.workers.read().await.keys().cloned().collect()
    }

    /// What every live worker is currently waiting on.
    ///
    /// The answer to "the UI is frozen": which sessions have a worker, whether
    /// it ever reported ready, and every RPC still outstanding against it with
    /// its age. A conversation stuck behind a materialization shows up as a
    /// worker that is not ready; one stuck in a tool shows up as a long-lived
    /// pending call.
    pub async fn waits(&self) -> Value {
        let workers = self.workers.read().await.clone();
        let mut rows: Vec<Value> = Vec::new();
        for (session, entry) in workers {
            let pending: Vec<Value> = entry
                .peer
                .in_flight()
                .into_iter()
                .map(|(id, method, age)| json!({ "id": id, "method": method, "age_s": age }))
                .collect();
            rows.push(json!({
                "session": session,
                "ready": *entry.ready.borrow(),
                "closed": entry.peer.is_closed(),
                "stopping": entry.stopping.load(std::sync::atomic::Ordering::SeqCst),
                "idle_s": entry
                    .last_activity
                    .lock()
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or_default(),
                "pending": pending,
            }));
        }
        rows.sort_by(|a, b| a["session"].as_str().cmp(&b["session"].as_str()));
        let materializing: Vec<String> = {
            let map = self.materializing.lock().await;
            map.iter()
                .filter(|(_, aspect)| aspect.try_lock().is_err())
                .map(|(session, _)| session.clone())
                .collect()
        };
        json!({ "workers": rows, "materializing": materializing })
    }

    /// Declares that this session's worker is about to be stopped on purpose,
    /// so its death is not filed as a crash. Safe to call for a session with
    /// no live worker.
    pub async fn mark_stopping(&self, session_id: &str) {
        if let Some(entry) = self.entry(session_id).await {
            entry
                .stopping
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The live peer for a session, if any — for operations that must not
    /// spawn a worker as a side effect (admin actions on stopped ones).
    pub async fn live_peer(&self, session_id: &str) -> Option<Arc<Peer>> {
        let entry = self.workers.read().await.get(session_id).cloned()?;
        (!entry.peer.is_closed()).then(|| entry.peer.clone())
    }

    /// Asks every live worker to stop, then waits briefly for the sockets to
    /// drain. The break-glass path before touching trunk's checkout.
    pub async fn stop_all(&self) {
        let workers = self.workers.read().await.clone();
        for (session, entry) in workers {
            tracing::info!(session = %session, "stopping worker (admin)");
            entry
                .stopping
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = entry.peer.call("shutdown", Value::Null).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    async fn entry(&self, session_id: &str) -> Option<Arc<WorkerEntry>> {
        self.workers.read().await.get(session_id).cloned()
    }

    /// This session's materialization lock, created on demand.
    ///
    /// The map is swept of unheld entries as it grows, so a gateway that has
    /// served thousands of conversations does not keep a mutex for each.
    async fn materializing_aspect(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.materializing.lock().await;
        if map.len() > 64 {
            map.retain(|_, aspect| Arc::strong_count(aspect) > 1);
        }
        map.entry(session_id.to_string()).or_default().clone()
    }

    async fn remove(&self, session_id: &str, peer: &Arc<Peer>) {
        let mut workers = self.workers.write().await;
        // Only remove the entry we own: a respawn may already have replaced it.
        if let Some(current) = workers.get(session_id) {
            if Arc::ptr_eq(&current.peer, peer) {
                workers.remove(session_id);
            }
        }
    }
}

/// Sends `method` to the session's worker, materializing branch, worktree,
/// and worker first if need be. This is the gateway's one entry point into a
/// conversation's runtime.
pub async fn call_session(
    grip: &Arc<Grip>,
    router: &Arc<WorkerRouter>,
    session_id: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let entry = ensure_worker(grip, router, session_id).await?;
    if let Ok(mut stamp) = entry.last_activity.lock() {
        *stamp = Instant::now();
    }
    entry.peer.call_within(method, params, method_budget(method)).await
}

/// How long this method is allowed to take.
///
/// The branch operations shell out to git, whose own per-command timeout is
/// 120s, and they run a sequence of those — so the default 60s was shorter
/// than one of their steps. Abandoning the call does not stop the work; it
/// only stops anyone hearing how it went, which is how a merge that had
/// already landed came back as a failure.
fn method_budget(method: &str) -> Duration {
    if method.starts_with("branch.") {
        ipc::SLOW_CALL_TIMEOUT
    } else {
        ipc::CALL_TIMEOUT
    }
}

/// The session's worker, spawned (and its branch materialized) on first use.
async fn ensure_worker(
    grip: &Arc<Grip>,
    router: &Arc<WorkerRouter>,
    session_id: &str,
) -> Result<Arc<WorkerEntry>> {
    if let Some(entry) = router.entry(session_id).await {
        if !entry.peer.is_closed() {
            return wait_ready(session_id, entry).await;
        }
    }

    let entry = {
        let aspect = router.materializing_aspect(session_id).await;
        let _guard = aspect.lock().await;
        // Re-check under the guard: whoever held it may have just spawned it.
        match router.entry(session_id).await {
            Some(entry) if !entry.peer.is_closed() => entry,
            _ => materialize(grip, router, session_id).await?,
        }
    };
    // Outside the guard. Waiting for a worker to answer is not a critical
    // section — the entry is already published, so a concurrent caller
    // rendezvouses on it through the fast path above.
    wait_ready(session_id, entry).await
}

/// Creates the branch, spawns the process, and publishes the router entry.
/// The caller holds this session's materialization lock.
async fn materialize(
    grip: &Arc<Grip>,
    router: &Arc<WorkerRouter>,
    session_id: &str,
) -> Result<Arc<WorkerEntry>> {
    // A conversation that keeps killing its worker gets no automatic third
    // try; the next human message resets the count.
    {
        let mut deaths = router
            .deaths
            .lock()
            .map_err(|_| anyhow::anyhow!("death ledger poisoned"))?;
        if deaths.fast_deaths.get(session_id).copied().unwrap_or(0) >= 2 {
            deaths.fast_deaths.remove(session_id);
            bail!(
                "this conversation's worker died twice within {}s of starting; \
                 not restarting it automatically. Its branch can be inspected or \
                 reset from /admin.",
                MIN_UPTIME.as_secs()
            );
        }
        deaths
            .last_start
            .insert(session_id.to_string(), Instant::now());
    }

    let store = grip
        .local_store()
        .context("only the gateway can spawn workers")?;
    let branches = Branches::new(grip.cfg.clone(), store.clone());

    // The chosen starting revision, if the user picked one before the first
    // message pinned the branch.
    let base = store.kv_get(session_id, PENDING_BASE_KEY)?.filter(|b| !b.is_empty());
    let row = branches.ensure(session_id, base.as_deref()).await?;
    let _ = store.kv_put(session_id, PENDING_BASE_KEY, "");

    let trunk = branches
        .root_git()
        .current_branch()
        .await
        .unwrap_or_else(|_| "main".to_string());
    let kernel = branch_kernel_path(grip, &row);
    let (stream, mut child) =
        spawn_worker_process(grip, session_id, &row.worktree, &trunk, kernel.as_deref())?;
    let pid = child.id().unwrap_or(0);

    let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
    let handler = Arc::new(GatewayHandler {
        grip: grip.clone(),
        session_id: session_id.to_string(),
        ready: ready_tx,
    });
    let (peer, connection_done) = Peer::spawn(stream, handler);
    // The read loop must be running before anything can be *answered* —
    // including our own handshake's response.
    let connection = tokio::spawn(connection_done);

    if let Err(e) = ipc::handshake(&peer, "gateway").await {
        // A branch kernel that no longer speaks our protocol is unusable as a
        // worker even if it runs; point the branch back at trunk so the next
        // attempt works, and say what happened.
        if kernel.is_some() && clear_branch_kernel(grip, session_id) {
            let _ = child.start_kill();
            bail!(
                "the branch's rebuilt kernel does not speak the gateway's protocol ({e:#});                  it was set aside — send the message again to continue on the trunk kernel"
            );
        }
        let _ = child.start_kill();
        return Err(e);
    }

    let entry = Arc::new(WorkerEntry {
        peer: peer.clone(),
        ready: ready_rx,
        last_activity: std::sync::Mutex::new(Instant::now()),
        stopping: std::sync::atomic::AtomicBool::new(false),
    });
    router
        .workers
        .write()
        .await
        .insert(session_id.to_string(), entry.clone());
    tracing::info!(session = %session_id, pid, branch = %row.branch_ref, "worker is up");

    // Watch the worker for the rest of its life.
    tokio::spawn(supervise(
        grip.clone(),
        router.clone(),
        session_id.to_string(),
        peer,
        child,
        connection,
    ));

    Ok(entry)
}

async fn wait_ready(session_id: &str, entry: Arc<WorkerEntry>) -> Result<Arc<WorkerEntry>> {
    let mut ready = entry.ready.clone();
    let wait = async {
        while !*ready.borrow() {
            ready.changed().await.map_err(|_| ())?;
        }
        Ok::<(), ()>(())
    };
    match tokio::time::timeout(READY_TIMEOUT, wait).await {
        Ok(Ok(())) => Ok(entry),
        Ok(Err(())) => bail!("the worker for {session_id} died while starting"),
        Err(_) => bail!(
            "the worker for {session_id} did not become ready within {}s",
            READY_TIMEOUT.as_secs()
        ),
    }
}

/// Runs from a worker's birth to its death: reaps the child, updates the
/// ledger, and hands interrupted turns to reconciliation.
async fn supervise(
    grip: Arc<Grip>,
    router: Arc<WorkerRouter>,
    session_id: String,
    peer: Arc<Peer>,
    mut child: tokio::process::Child,
    connection: tokio::task::JoinHandle<()>,
) {
    // Death is whichever comes first: the socket closing, or the process
    // exiting. Watching only the socket used to be enough, but any grandchild
    // that inherits it (a shell the agent left running) keeps it open
    // indefinitely — and this task, and with it the router entry, would never
    // advance. The exit status is the authoritative signal; EOF is a hint.
    let mut connection = connection;
    tokio::select! {
        _ = &mut connection => {}
        _ = child.wait() => {
            // The process is gone but something still holds its end of the
            // socket. Drop the read loop rather than leaving it parked on a
            // socket that will never EOF.
            connection.abort();
        }
    }
    // Whichever arm won, callers still parked on this peer must be woken now
    // rather than at CALL_TIMEOUT, and no new call may be admitted.
    peer.force_close();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    let requested = router
        .entry(&session_id)
        .await
        .map(|e| e.stopping.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(false);
    router.remove(&session_id, &peer).await;

    // A stop we asked for is not a crash, however quickly it followed the
    // worker's birth.
    if requested {
        if let Ok(mut deaths) = router.deaths.lock() {
            deaths.fast_deaths.remove(&session_id);
        }
    }

    let fast = if requested {
        0
    } else {
        let mut deaths = match router.deaths.lock() {
            Ok(deaths) => deaths,
            Err(_) => return,
        };
        let lived = deaths
            .last_start
            .get(&session_id)
            .map(|s| s.elapsed())
            .unwrap_or(MIN_UPTIME);
        if lived < MIN_UPTIME {
            let n = deaths.fast_deaths.entry(session_id.clone()).or_insert(0);
            *n += 1;
            *n
        } else {
            deaths.fast_deaths.remove(&session_id);
            0
        }
    };

    if fast >= 2 {
        // A branch-built kernel that keeps dying is abandoned in favour of
        // the trunk binary; only when trunk's own kernel crash-loops does the
        // conversation actually stay down.
        let fell_back = clear_branch_kernel(&grip, &session_id);
        if fell_back {
            if let Ok(mut deaths) = router.deaths.lock() {
                deaths.fast_deaths.remove(&session_id);
            }
            tracing::warn!(session = %session_id, "branch kernel abandoned after crash-looping");
            let _ = grip
                .append_event(
                    &session_id,
                    crate::bindings::types::SessionEvent::Incident(
                        "This branch's rebuilt kernel kept crashing at startup, so the \
                         conversation is back on the trunk kernel. The branch source is \
                         untouched — fix it and restart again."
                            .to_string(),
                    ),
                )
                .await;
            reconcile_session(&grip, &session_id).await;
            return;
        }

        tracing::error!(
            session = %session_id,
            "worker died twice within {}s of starting; leaving it down",
            MIN_UPTIME.as_secs()
        );
        let _ = grip
            .append_event(
                &session_id,
                crate::bindings::types::SessionEvent::Incident(
                    "This conversation's runtime keeps crashing at startup, so it was not \
                     restarted. Its branch is intact and can be reset from /admin."
                        .to_string(),
                ),
            )
            .await;
        return;
    }

    tracing::info!(session = %session_id, "worker exited");
    // Repair whatever *this* death interrupted; resuming re-materializes the
    // worker on demand. Deliberately scoped: see `reconcile_session`.
    reconcile_session(&grip, &session_id).await;
}

/// Points a branch back at the trunk kernel. True when it was on its own.
fn clear_branch_kernel(grip: &Arc<Grip>, session_id: &str) -> bool {
    let Some(store) = grip.local_store() else {
        return false;
    };
    let branches = Branches::new(grip.cfg.clone(), store.clone());
    match branches.get(session_id) {
        Ok(Some(mut row)) if !row.kernel_commit.is_empty() => {
            row.kernel_commit = String::new();
            branches.update(&row).is_ok()
        }
        _ => false,
    }
}

/// Stops workers whose conversations have gone quiet.
pub fn spawn_reaper(router: Arc<WorkerRouter>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        // A sweep that overran its minute must not be followed by a burst of
        // catch-up sweeps.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let workers = router.workers.read().await.clone();
            // Asked concurrently. A wedged worker costs a full CALL_TIMEOUT to
            // ask, and serially that was 60s *each* — one unresponsive
            // conversation could hold the sweep past the next tick and leave
            // genuinely idle workers running indefinitely.
            let mut asking = tokio::task::JoinSet::new();
            for (session, entry) in workers {
                let idle = entry
                    .last_activity
                    .lock()
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                if idle < IDLE_TIMEOUT {
                    continue;
                }
                asking.spawn(async move {
                    // Ask, don't kill: a worker mid-turn or mid-build declines,
                    // and that answer is itself activity — the clock restarts.
                    entry
                        .stopping
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    match entry
                        .peer
                        .call("shutdown", serde_json::json!({ "if_idle": true }))
                        .await
                    {
                        Ok(reply) if reply.get("busy").and_then(Value::as_bool) == Some(true) => {
                            entry
                                .stopping
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            if let Ok(mut stamp) = entry.last_activity.lock() {
                                *stamp = Instant::now();
                            }
                            tracing::debug!(session = %session, "idle check: worker is busy; leaving it");
                        }
                        _ => {
                            tracing::info!(session = %session, idle_secs = idle.as_secs(), "stopped an idle worker");
                        }
                    }
                });
            }
            while asking.join_next().await.is_some() {}
        }
    });
}

/// What the gateway does with traffic a worker initiates: store requests,
/// rendered frames, readiness, restart requests.
pub struct GatewayHandler {
    pub grip: Arc<Grip>,
    pub session_id: String,
    ready: tokio::sync::watch::Sender<bool>,
}

impl ipc::Handler for GatewayHandler {
    fn handle(
        self: Arc<Self>,
        method: String,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            if method == "hello" {
                return Ok(ipc::hello_response());
            }
            if method.starts_with("store.") {
                let store = self
                    .grip
                    .local_store()
                    .context("gateway has no local store")?;
                return crate::persist::serve_store_call(
                    store,
                    &method,
                    params,
                    &self.session_id,
                )
                .await;
            }
            anyhow::bail!("unknown gateway method {method}")
        })
    }

    fn handle_note(self: Arc<Self>, name: String, params: Value) {
        let grip = self.grip.clone();

        // Anything a worker volunteers — frames, events, readiness — proves it
        // is alive and working; the idle clock must not run against it.
        if let Role::Gateway(router) = &grip.role {
            let router = router.clone();
            let session = self.session_id.clone();
            tokio::spawn(async move {
                if let Some(entry) = router.entry(&session).await {
                    if let Ok(mut stamp) = entry.last_activity.lock() {
                        *stamp = Instant::now();
                    }
                }
            });
        }

        match name.as_str() {
            // A frame the worker rendered for one of its session's events.
            "frame" => {
                let session = params
                    .get("session")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let frame = params
                    .get("frame")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !session.is_empty() && !frame.is_empty() {
                    let _ = grip.frames_tx.send(RenderedFrame {
                        session_id: session,
                        frame,
                    });
                }
            }
            // The worker's raw event stream, mirrored so connectors on this
            // side (Discord) can follow conversations exactly as before the
            // split. Browsers are fed by the rendered `frame` notes instead.
            "event" => {
                match serde_json::from_value::<crate::bindings::types::OutboundEvent>(params) {
                    Ok(event) => {
                        let _ = grip.events_tx.send(event);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "unreadable event from the worker");
                    }
                }
            }
            // The worker's aspects are up and it is accepting turns.
            "ready" => {
                let _ = self.ready.send(true);
                let session_id = self.session_id.clone();
                tokio::spawn(async move {
                    // A fresh deployment serves the fallback page until some
                    // worker's first build lands in the cache; a branch at
                    // trunk's head keys identically to trunk, so try again.
                    let ui = crate::aspect::Aspect::gateway(&grip.cfg.primary_gateway);
                    if grip.loader.get(&ui).is_none() {
                        crate::roles::gateway::load_ui_gateway(&grip).await;
                    }
                    reconcile_session(&grip, &session_id).await;
                });
            }
            // The worker wants itself restarted — after a kernel rebuild in
            // its branch, or a config change read only at startup. Probe and
            // cache the new binary first; then a plain shutdown, because the
            // supervision path already knows how to resume what it interrupts.
            "restart_worker" => {
                let session = self.session_id.clone();
                let kernel = params
                    .get("kernel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let reason = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("worker requested restart")
                    .to_string();
                tracing::info!(session = %session, %reason, "worker restart requested");
                tokio::spawn(async move {
                    // The worker that asked, captured now. Adoption below
                    // probes and copies a binary and can take minutes; by the
                    // time it returns, this session may already have died and
                    // been replaced, and shutting down "the worker for this
                    // session" would then kill an innocent replacement
                    // mid-turn.
                    let asked_by = match &grip.role {
                        crate::grip::Role::Gateway(router) => {
                            router.live_peer(&session).await
                        }
                        _ => None,
                    };
                    if let Some(kernel) = kernel {
                        if let Err(e) = adopt_branch_kernel(&grip, &session, &kernel).await {
                            tracing::warn!(session = %session, error = %e,
                                "the branch kernel was not adopted; restarting on the old one");
                            let _ = grip
                                .append_event(
                                    &session,
                                    crate::bindings::types::SessionEvent::Incident(format!(
                                        "The rebuilt kernel was not adopted: {e:#}.                                          Restarting on the previous one."
                                    )),
                                )
                                .await;
                        }
                    }
                    if let crate::grip::Role::Gateway(router) = &grip.role {
                        match (asked_by, router.live_peer(&session).await) {
                            (Some(asked), Some(live)) if Arc::ptr_eq(&asked, &live) => {
                                router.mark_stopping(&session).await;
                                let _ = live.call("shutdown", Value::Null).await;
                            }
                            (Some(_), _) => {
                                tracing::info!(session = %session,
                                    "the worker that asked for a restart is already gone; \
                                     leaving its replacement alone");
                            }
                            // No peer was captured (the ask arrived before the
                            // entry was published): fall back to the old
                            // behaviour rather than skipping the restart.
                            (None, Some(live)) => {
                                router.mark_stopping(&session).await;
                                let _ = live.call("shutdown", Value::Null).await;
                            }
                            (None, None) => {}
                        }
                    }
                });
            }
            other => {
                tracing::debug!(note = other, "ignoring unknown worker note");
            }
        }
    }
}

/// Repairs turns cut short by a crash or restart, then carries them on.
///
/// Runs on the gateway (it owns the log) at boot, whenever a worker reports
/// ready, and whenever one dies. Attempt counts are persisted, so a
/// crash-looping turn stops being resumed after a couple of tries rather
/// than crashing its worker forever.
/// Returns a boxed future because it sits on a cycle — reconciling resumes
/// turns, resuming materializes workers, and a worker's supervision calls
/// back here when it dies. The box gives the compiler a concrete type to
/// close the loop on.
pub fn reconcile_and_resume(
    grip: &Arc<Grip>,
) -> futures_util::future::BoxFuture<'static, ()> {
    let grip = grip.clone();
    Box::pin(async move { reconcile_and_resume_inner(grip, None).await })
}

/// Reconciles one conversation, for the common case: its own worker died.
///
/// A worker's death says nothing about anyone else, and the fleet-wide sweep
/// used to drag in every session that merely had no worker at that instant —
/// including agents part-way through a restart they asked for. With several
/// self-modifying conversations running that fed back on itself.
pub fn reconcile_session(
    grip: &Arc<Grip>,
    session_id: &str,
) -> futures_util::future::BoxFuture<'static, ()> {
    let grip = grip.clone();
    let session_id = session_id.to_string();
    Box::pin(async move { reconcile_and_resume_inner(grip, Some(session_id)).await })
}

async fn reconcile_and_resume_inner(grip: Arc<Grip>, only: Option<String>) {
    // One at a time: readiness and death fire this from several places, and
    // two scans racing each other once synthesized duplicate tool results.
    static RECONCILING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = RECONCILING.lock().await;

    let Some(store) = grip.local_store() else {
        return;
    };
    // Sessions with a live worker are that worker's business; only sessions
    // nothing is running can hold a genuinely interrupted turn.
    let live = match &grip.role {
        Role::Gateway(router) => router.live_sessions().await,
        Role::Worker(_) => Vec::new(),
    };
    let interrupted = match store.reconcile_interrupted_turns(
        "This turn was interrupted and has been picked back up. Carry on from where \
         you left off; anything you were part-way through may need doing again.",
        &live,
        only.as_deref(),
    ) {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "could not reconcile interrupted turns");
            return;
        }
    };
    if interrupted.is_empty() {
        return;
    }

    let resuming: Vec<String> = interrupted
        .iter()
        .filter(|i| i.resume)
        .map(|i| i.session_id.clone())
        .collect();

    tracing::info!(
        interrupted = interrupted.len(),
        resuming = resuming.len(),
        "reconciled interrupted turns"
    );

    // The scan is what must not race; the resumes are not. Each `resume`
    // materializes a worker, so holding the guard across this loop meant a
    // boot with five interrupted turns could sit here for the better part of
    // an hour with every other reconciliation queued behind it — including
    // the ones triggered by workers dying in the meantime.
    drop(_one_at_a_time);

    let mut resumes = tokio::task::JoinSet::new();
    let mut queue = resuming.into_iter();
    let mut in_flight = 0usize;
    loop {
        while in_flight < RESUME_CONCURRENCY {
            let Some(session_id) = queue.next() else { break };
            let grip = grip.clone();
            resumes.spawn(async move {
                tracing::info!(session = %session_id, "resuming");
                // Bounded so one wedged branch cannot hold an aspect forever;
                // `resume` is itself capped by READY_TIMEOUT, and this is the
                // outer guard for everything after it.
                if tokio::time::timeout(RESUME_TIMEOUT, grip.resume(&session_id))
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        session = %session_id,
                        "gave up resuming after {}s; it will be retried on the next message",
                        RESUME_TIMEOUT.as_secs()
                    );
                }
            });
            in_flight += 1;
        }
        if in_flight == 0 {
            break;
        }
        let _ = resumes.join_next().await;
        in_flight -= 1;
    }
}

/// The cached kernel this branch runs, when it has adopted one and the cache
/// still holds it. `None` means the trunk binary — this very executable.
fn branch_kernel_path(grip: &Arc<Grip>, row: &crate::branches::BranchRow) -> Option<std::path::PathBuf> {
    if row.kernel_commit.is_empty() {
        return None;
    }
    let path = grip
        .cfg
        .paths
        .artifacts
        .join("cache/kernel")
        .join(&row.kernel_commit)
        .join("thetis");
    path.is_file().then_some(path)
}

/// Probes a branch-built kernel and, if it answers, files it in the kernel
/// cache and points the branch at it. The next spawn of this worker runs it.
async fn adopt_branch_kernel(
    grip: &Arc<Grip>,
    session_id: &str,
    kernel: &str,
) -> Result<()> {
    let kernel = std::path::Path::new(kernel);
    if !kernel.is_file() {
        bail!("{} does not exist", kernel.display());
    }

    // Will it even start and speak our protocol? A binary that cannot answer
    // the probe would take the conversation down with it. This used to be a
    // verbatim copy of `control::probe_kernel` that had drifted — it never
    // killed a probe that hung.
    crate::control::probe_kernel(kernel).await?;

    let store = grip.local_store().context("gateway only")?;
    let branches = Branches::new(grip.cfg.clone(), store.clone());
    let mut row = branches
        .get(session_id)?
        .context("no branch to adopt a kernel for")?;

    // Keyed by the branch's current commit: the kernel is a build of it.
    let commit = crate::gitctl::GitCtl::new(&row.worktree).head().await?;
    let dir = grip.cfg.paths.artifacts.join("cache/kernel").join(&commit);
    std::fs::create_dir_all(&dir)?;
    let cached = dir.join("thetis");

    // Stage beside it and rename into place, rather than writing over it.
    //
    // The cache path is keyed by the branch's commit, so a second restart at
    // the same commit — a config change, a retry — lands on this exact file
    // again, and by then a worker is very likely *executing* it. Copying onto
    // a running executable fails with ETXTBSY and the whole adoption is
    // abandoned, leaving the agent on the old kernel with no way forward.
    // Renaming swaps the directory entry instead: the running process keeps
    // the inode it started from, and the next exec picks up the new one.
    // Unique per attempt as well as per session: two adoptions for the same
    // session at the same commit — a retry after a slow probe, say — would
    // otherwise copy into the same staging file concurrently and rename a
    // half-written binary into place.
    let attempt = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = dir.join(format!("thetis.incoming.{session_id}.{attempt}"));
    std::fs::copy(kernel, &staging)
        .with_context(|| format!("staging the kernel at {}", staging.display()))?;
    if let Err(e) = std::fs::rename(&staging, &cached) {
        let _ = std::fs::remove_file(&staging);
        return Err(e)
            .with_context(|| format!("caching the kernel at {}", cached.display()));
    }

    row.kernel_commit = commit.clone();
    branches.update(&row)?;
    tracing::info!(
        session = %session_id,
        commit = %&commit[..12.min(commit.len())],
        "branch kernel adopted"
    );
    Ok(())
}

fn spawn_worker_process(
    grip: &Arc<Grip>,
    session_id: &str,
    worktree: &std::path::Path,
    trunk: &str,
    kernel: Option<&std::path::Path>,
) -> Result<(tokio::net::UnixStream, tokio::process::Child)> {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().context("socketpair")?;
    let theirs_fd = theirs.into_raw_fd();

    let cfg = &grip.cfg;
    let exe = match kernel {
        Some(kernel) => {
            tracing::info!(session = %session_id, kernel = %kernel.display(), "spawning on a branch kernel");
            kernel.to_path_buf()
        }
        // Pinned at startup rather than read from /proc now: a rebuild during
        // this gateway's life unlinks the file it was started from, and every
        // conversation opened afterwards would fail to spawn with ENOENT.
        None => crate::control::self_exe()?,
    };
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("worker")
        .arg("--session")
        .arg(session_id)
        .arg("--worktree")
        .arg(worktree)
        .current_dir(worktree)
        // Shared state is pinned over the environment so a branch cannot
        // retarget it by editing its own copy of thetis.toml.
        .env("THETIS_DATA_DIR", &cfg.paths.data)
        .env("THETIS_ARTIFACTS_DIR", &cfg.paths.artifacts)
        .env("THETIS_TARGET_DIR", &cfg.build.target_dir)
        .env("THETIS_LOCAL_CONFIG", cfg.local_overlay())
        .env("THETIS_TRUNK", trunk)
        // A worker must never mistake itself for a supervised service: its
        // restart path is the gateway, not systemd.
        .env_remove("INVOCATION_ID")
        .kill_on_drop(true);
    if let Some(workspace) = cfg.wasi.dirs.first() {
        cmd.env("THETIS_WORKSPACE_DIR", workspace);
    }

    // After fork, before exec: pin the socket to the agreed fd and arrange to
    // die with the gateway. Only async-signal-safe calls are allowed here.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(theirs_fd, WORKER_SOCKET_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawning worker")?;

    // Close our copy of the worker's end, or its death would never read as EOF.
    unsafe {
        libc::close(theirs_fd);
    }

    ours.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(ours)?;
    Ok((stream, child))
}
