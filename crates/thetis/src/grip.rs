//! The grip: everything the orchestrator knows how to do, in one place.
//!
//! Peleus won Thetis by holding on through every shape she took. This is that
//! hold: the one surface that stays constant while the aspects around it are
//! rebuilt, swapped and rolled back underneath it.
//!
//! Host imports, session actors, and the web layer all reach the system through
//! an `Arc<Grip>`. It owns the database, the LLM client, the component
//! registry, and the event fan-out to connected browsers.

use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::aspect::Aspect;
use crate::bindings::types::{Attachment, OutboundEvent, SessionEvent, ToolManifest, TurnStats};
use crate::builder::Builder;
use crate::config::Config;
use crate::llm::LlmClient;
use crate::loader::Loader;
use crate::persist::Persist;
use crate::revisions::Revisions;
use crate::runtime::{Budget, Caps, Runtime};
use crate::session::SessionActors;
use crate::store::Store;
use crate::watchdog::Breakers;

/// Which half of the process split this grip is running.
///
/// The gateway owns the browsers and the database and routes conversations to
/// workers; a worker owns the wasm runtime and the toolchain and runs the
/// conversations themselves. Both are the same binary and share this type —
/// the role decides where turns run and where state lives.
pub enum Role {
    Gateway(Arc<crate::workers::WorkerRouter>),
    Worker(Arc<crate::ipc::Peer>),
}

/// Why a turn ended badly.
///
/// The distinction matters: a reported error usually means something outside
/// the agent went wrong (the model refused, the key is missing), while a trap
/// means this revision of the agent is itself faulty — only traps should count
/// against its circuit breaker.
#[derive(Debug, Clone)]
pub enum TurnError {
    Reported(String),
    Trapped(String),
}

impl TurnError {
    pub fn message(&self) -> &str {
        match self {
            TurnError::Reported(m) | TurnError::Trapped(m) => m,
        }
    }

    pub fn is_trap(&self) -> bool {
        matches!(self, TurnError::Trapped(_))
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnError::Reported(m) => write!(f, "{m}"),
            TurnError::Trapped(m) => write!(f, "the agent trapped: {m}"),
        }
    }
}

/// A session event already rendered to a wire frame by the gateway.
#[derive(Debug, Clone)]
pub struct RenderedFrame {
    pub session_id: String,
    pub frame: String,
}

pub struct Grip {
    pub cfg: Arc<Config>,
    pub persist: Persist,
    pub role: Role,
    /// This worker's checkout. Every green build, skill edit, and turn end
    /// becomes a commit on the conversation's branch — the branch history IS
    /// the revision history. `None` on the gateway.
    pub git: Option<crate::gitctl::GitCtl>,
    pub llm: Arc<LlmClient>,
    pub loader: Arc<Loader>,
    pub runtime: Arc<Runtime>,
    pub revisions: Arc<Revisions>,
    /// Content-addressed build artifacts, shared across every branch: the
    /// same source tree builds once no matter how many conversations hold it.
    pub buildcache: crate::buildcache::BuildCache,
    pub builder: Arc<Builder>,
    /// Raw events, consumed by the renderer task.
    pub events_tx: broadcast::Sender<OutboundEvent>,
    /// Rendered frames, consumed by websocket connections.
    pub frames_tx: broadcast::Sender<RenderedFrame>,
    pub sessions: SessionActors,
    pub breakers: Breakers,
    pub terminals: crate::terminal::Terminals,
    /// Skill discovery, retrieval and editing. Holds a cached tree, so it is
    /// shared rather than constructed per call.
    pub skills: Arc<crate::skill_manager::SkillManager>,
    /// Aspects whose file-change events should be ignored until the given time.
    /// A rollback rewrites the source tree, and without this the watcher would
    /// treat its own restore as a fresh edit and rebuild over it.
    watch_suppressed_until: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// Manifests of the loaded tool components, captured when each was
    /// validated. Cached because the agent reads the whole list on every loop
    /// iteration, and calling into each tool for it would be wasteful.
    tool_manifests: std::sync::RwLock<std::collections::HashMap<String, ToolManifest>>,
    /// Aspects with a build in flight.
    ///
    /// Builds serialize on one lock, so an agent that keeps asking for the same
    /// aspect would otherwise queue work behind a build that is already going to
    /// supersede it. Refusing the duplicate keeps the queue bounded no matter
    /// how the agent loops.
    building: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Aspects whose source changed while a build for them was already running.
    ///
    /// That request used to be logged and dropped, so the *newest* edit was
    /// the one that never got built — the agent's last change silently did not
    /// take effect, and nothing said so.
    rebuild_wanted: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Source fingerprints that already failed to build, per aspect.
    ///
    /// Without this a failing tree is retried on every file event forever:
    /// a failure never commits and never reaches the build cache, so nothing
    /// upstream remembers it and the next event looks brand new. One agent
    /// stuck on a contract mismatch held a core at 100% for hours this way.
    ///
    /// Keyed by the aspect, holding the fingerprint of the source that failed
    /// and why. A *different* fingerprint always builds — the point is to
    /// refuse repeats of known-bad input, never to refuse new work.
    build_failures: std::sync::Mutex<std::collections::HashMap<String, FailedBuild>>,
    /// Turns currently in flight. What "idle" means for a worker: a long
    /// agentic turn generates no inbound traffic, and must not read as quiet.
    turns_running: std::sync::atomic::AtomicUsize,
    /// Stamps each turn-count report to the gateway. Notes are fire and
    /// forget and each is sent from its own task, so two reports can land out
    /// of order — and a stale "1" arriving after the "0" that ended the turn
    /// would leave the gateway showing a turn that finished. The receiver
    /// keeps the highest stamp it has seen and drops anything older.
    turn_report_seq: std::sync::atomic::AtomicU64,
    /// Rung when a sub-agent finishes, so a parent parked in `delegation::wait`
    /// wakes at once rather than at its next backstop poll. Worker-local, which
    /// is sufficient because a child always runs in its parent's worker.
    pub settle_bell: crate::delegation::SettleBell,
}

/// A build that failed, remembered so the identical source is not retried.
#[derive(Clone)]
pub struct FailedBuild {
    /// Fingerprint of the source tree that failed.
    pub fingerprint: String,
    /// Why it failed, for the message when a retry is refused.
    pub detail: String,
}

/// Marks a turn as running for as long as it is held.
pub struct TurnGuard {
    grip: Arc<Grip>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let running = self
            .grip
            .turns_running
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_sub(1);
        self.grip.report_turns(running);
    }
}

/// Marks an aspect as building for as long as it is held.
pub struct BuildGuard {
    grip: Arc<Grip>,
    aspect: String,
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.grip.building.lock() {
            in_flight.remove(&self.aspect);
        }
        // Anything that arrived while this build held the aspect is now owed a
        // build of its own. Ordering matters: the aspect is released above, so
        // the task spawned here can claim it.
        let wanted = self
            .grip
            .rebuild_wanted
            .lock()
            .map(|mut w| w.remove(&self.aspect))
            .unwrap_or(false);
        if !wanted {
            return;
        }
        let Ok(aspect) = crate::aspect::Aspect::parse(&self.aspect) else {
            return;
        };
        // A guard can be dropped outside a runtime (tests, shutdown paths),
        // where `spawn` would panic inside a destructor.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let grip = self.grip.clone();
        runtime.spawn(async move {
            tracing::info!(aspect = %aspect, "rebuilding for a change that arrived mid-build");
            if let Err(e) = crate::pipeline::build_and_activate(
                &grip,
                &aspect,
                crate::revisions::Origin::HumanEdit,
                "a change arrived while the previous build was running",
            )
            .await
            {
                tracing::warn!(aspect = %aspect, error = %e, "the follow-up rebuild failed");
            }
        });
    }
}

impl Grip {
    /// The gateway-role grip: the process that owns the database, the
    /// listener, and the worker fleet.
    pub fn gateway(
        cfg: Arc<Config>,
        runtime: Arc<Runtime>,
        db: Arc<Store>,
        router: Arc<crate::workers::WorkerRouter>,
    ) -> Result<Arc<Self>> {
        Self::build(cfg, runtime, Persist::Local(db), Role::Gateway(router))
    }

    /// A worker-role grip: one conversation runtime, persisting through
    /// the gateway it inherited a socket to.
    pub fn worker(
        cfg: Arc<Config>,
        runtime: Arc<Runtime>,
        gateway: Arc<crate::ipc::Peer>,
    ) -> Result<Arc<Self>> {
        Self::build(
            cfg,
            runtime,
            Persist::Remote(gateway.clone()),
            Role::Worker(gateway),
        )
    }

    fn build(
        cfg: Arc<Config>,
        runtime: Arc<Runtime>,
        persist: Persist,
        role: Role,
    ) -> Result<Arc<Self>> {
        let git = match role {
            Role::Worker(_) => Some(crate::gitctl::GitCtl::new(cfg.root.clone())),
            Role::Gateway(_) => None,
        };
        let llm = Arc::new(LlmClient::new(cfg.clone())?);
        let (events_tx, _) = broadcast::channel(1024);
        let (frames_tx, _) = broadcast::channel(1024);

        let revisions = Arc::new(Revisions::new(cfg.clone(), persist.clone()));
        let skills = Arc::new(crate::skill_manager::SkillManager::new(
            cfg.clone(),
            persist.clone(),
        )?);
        let cfg_for_breakers = cfg.clone();

        Ok(Arc::new(Self {
            cfg,
            persist,
            role,
            git,
            llm,
            loader: Arc::new(Loader::new()),
            runtime,
            revisions,
            buildcache: crate::buildcache::BuildCache::new(
                cfg_for_breakers.paths.artifacts.join("cache"),
            ),
            builder: Arc::new(Builder::new()),
            events_tx,
            frames_tx,
            sessions: SessionActors::new(),
            terminals: crate::terminal::Terminals::new(),
            skills,
            breakers: Breakers::new(
                cfg_for_breakers.watchdog.failure_window,
                cfg_for_breakers.watchdog.failure_threshold,
            ),
            watch_suppressed_until: std::sync::Mutex::new(std::collections::HashMap::new()),
            tool_manifests: std::sync::RwLock::new(std::collections::HashMap::new()),
            building: std::sync::Mutex::new(std::collections::HashSet::new()),
            rebuild_wanted: std::sync::Mutex::new(std::collections::HashSet::new()),
            build_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
            turns_running: std::sync::atomic::AtomicUsize::new(0),
            turn_report_seq: std::sync::atomic::AtomicU64::new(0),
            settle_bell: crate::delegation::SettleBell::default(),
        }))
    }

    /// Commits everything in this worker's checkout, if anything changed.
    ///
    /// The one write path onto the branch: builds, skill edits, and turn-end
    /// checkpoints all funnel through here, so nothing on a branch is ever
    /// more than one commit away from `git log`.
    pub async fn commit_worktree(&self, message: &str) -> Result<Option<String>> {
        let Some(git) = &self.git else {
            return Ok(None);
        };
        match git.add_all_and_commit(message).await {
            Ok(commit) => {
                if let Some(rev) = &commit {
                    tracing::debug!(rev = %&rev[..12.min(rev.len())], message, "committed");
                }
                Ok(commit)
            }
            Err(e) => {
                // A failed checkpoint loses granularity, not work: the files
                // are still on disk and the next checkpoint sweeps them up.
                tracing::warn!(error = %e, "could not commit the worktree");
                Err(e)
            }
        }
    }

    /// The database, on the one process allowed to touch it directly.
    pub fn local_store(&self) -> Option<&Arc<Store>> {
        match &self.persist {
            Persist::Local(store) => Some(store),
            Persist::Remote(_) => None,
        }
    }

    /// Marks a turn as running until the guard drops.
    pub fn begin_turn(self: &Arc<Self>) -> TurnGuard {
        let running = self
            .turns_running
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.report_turns(running);
        TurnGuard { grip: self.clone() }
    }

    /// Tells the gateway how many turns this worker is running.
    ///
    /// The counter is per process, and turns run in workers, so the gateway's
    /// own was always zero — `/admin/waits` reported "nothing is running"
    /// while a worker was twelve minutes into a turn, which is exactly the
    /// question that page exists to answer. Pushed rather than polled so the
    /// admin page cannot block on a worker that is itself wedged.
    fn report_turns(self: &Arc<Self>, running: usize) {
        let Role::Worker(peer) = &self.role else {
            return;
        };
        // A guard can be dropped from anywhere, including an unwind off the
        // runtime; without a handle there is nothing to spawn onto and the
        // count simply goes unreported.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // Stamped before the spawn, so the order the reports were *made* in
        // survives the order their tasks happen to run in.
        let seq = self
            .turn_report_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let peer = peer.clone();
        handle.spawn(async move {
            peer.notify(
                "turns",
                serde_json::json!({ "running": running, "seq": seq }),
            )
            .await;
        });
    }

    /// Whether a turn is running right now.
    ///
    /// Narrower than [`is_busy`] on purpose: it excludes builds, so a caller
    /// can ask "is the agent working?" without its own build answering yes.
    pub fn turn_in_flight(&self) -> bool {
        self.turns_running.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// Whether this process is in the middle of anything a shutdown would
    /// interrupt: a turn, or a build.
    /// Aspects with a build in flight, for `/admin/waits`.
    pub fn building_aspects(&self) -> Vec<String> {
        self.building
            .lock()
            .map(|b| {
                let mut v: Vec<String> = b.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// How many turns are running in this process, for `/admin/waits`.
    pub fn turns_in_flight(&self) -> usize {
        self.turns_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_busy(&self) -> bool {
        if self.turns_running.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            return true;
        }
        self.building.lock().map(|b| !b.is_empty()).unwrap_or(false)
    }

    /// Claims the right to build an aspect, or `None` if one is already running.
    pub fn begin_build(self: &Arc<Self>, aspect: &Aspect) -> Option<BuildGuard> {
        let mut in_flight = self.building.lock().ok()?;
        if !in_flight.insert(aspect.key()) {
            // Remember that this aspect still needs building; the running
            // build's guard picks it up when it finishes.
            if let Ok(mut wanted) = self.rebuild_wanted.lock() {
                wanted.insert(aspect.key());
            }
            return None;
        }
        Some(BuildGuard {
            grip: self.clone(),
            aspect: aspect.key(),
        })
    }

    /// Why this exact source last failed to build, if it did.
    ///
    /// A fingerprint that does not match the remembered one returns `None`:
    /// the source moved on, so it deserves a real attempt.
    pub fn known_bad_build(&self, aspect: &Aspect, fingerprint: &str) -> Option<String> {
        let map = self.build_failures.lock().ok()?;
        let failed = map.get(&aspect.key())?;
        (failed.fingerprint == fingerprint).then(|| failed.detail.clone())
    }

    /// Remembers that this source failed, so an identical retry is refused.
    pub fn record_build_failure(&self, aspect: &Aspect, fingerprint: &str, detail: &str) {
        if let Ok(mut map) = self.build_failures.lock() {
            map.insert(
                aspect.key(),
                FailedBuild {
                    fingerprint: fingerprint.to_string(),
                    detail: detail.to_string(),
                },
            );
        }
    }

    /// Forgets an aspect's remembered failure, after a build succeeds.
    ///
    /// Also called when a build is *attempted* on new source, so a tree that
    /// fails differently each time still reports its newest error rather than
    /// the first one.
    pub fn clear_build_failure(&self, aspect: &Aspect) {
        if let Ok(mut map) = self.build_failures.lock() {
            map.remove(&aspect.key());
        }
    }

    // --- events ------------------------------------------------------------

    /// Persists an event and publishes it to connected clients.
    pub async fn append_event(&self, session_id: &str, event: SessionEvent) -> Result<u64> {
        let record = self.persist.append_event(session_id, event).await?;
        let _ = self.events_tx.send(OutboundEvent {
            session_id: session_id.to_string(),
            seq: Some(record.seq),
            ts_ms: record.ts_ms,
            event: record.event,
        });
        Ok(record.seq)
    }

    /// Publishes without persisting. Used for streaming token deltas, which
    /// would otherwise bloat the log with thousands of fragments.
    pub fn publish_transient(&self, session_id: &str, event: SessionEvent) {
        let _ = self.events_tx.send(OutboundEvent {
            session_id: session_id.to_string(),
            seq: None,
            ts_ms: crate::store::now_ms(),
            event,
        });
    }

    // --- policy-scoped stores ----------------------------------------------
    pub fn gateway_store(
        self: &Arc<Self>,
        budget: Budget,
        principal: Arc<crate::auth::Principal>,
    ) -> wasmtime::Store<crate::runtime::HostState> {
        let mut s = self
            .runtime
            .new_store(self.clone(), Caps::Gateway, budget, None);
        s.data_mut().policy = principal.policy.clone();
        s.data_mut().principal = Some(principal);
        s
    }
    pub async fn session_store(
        self: &Arc<Self>,
        caps: Caps,
        budget: Budget,
        id: &str,
    ) -> wasmtime::Store<crate::runtime::HostState> {
        let policy = if matches!(&self.role, Role::Worker(_)) {
            self.persist.session_policy(id).await.map(Arc::new).unwrap_or_else(|error| {
                tracing::warn!(session = id, %error, "could not load session policy; denying worker capabilities");
                let mut denied = self.cfg.auth.local_policy.as_ref().clone();
                denied.admin = false;
                denied.read_only = true;
                denied.see_all_sessions = false;
                denied.denied.extend(crate::policy::Cap::all().iter().copied());
                Arc::new(denied)
            })
        } else {
            match self.persist.owner_of_root(id).await {
                Ok(Some(owner)) => self.cfg.auth.policy_for(&owner),
                _ => self.cfg.auth.local_policy.clone(),
            }
        };
        let owner = self.persist.owner_of_root(id).await.ok().flatten();
        let principal = owner.map(|owner| {
            let user = self.cfg.auth.user(&owner);
            Arc::new(crate::auth::Principal::new(
                owner.clone(),
                user.map(|u| u.name.clone())
                    .unwrap_or_else(|| owner.clone()),
                user.map(|u| u.role.clone()).unwrap_or_default(),
                policy.clone(),
            ))
        });
        let mut s = self
            .runtime
            .new_store(self.clone(), caps, budget, Some(id.into()));
        s.data_mut().policy = policy;
        s.data_mut().principal = principal;
        s
    }

    // --- turns -------------------------------------------------------------

    /// Runs one agentic turn to completion inside a fresh store.
    ///
    /// A trap (guest panic, memory limit, blown budget) surfaces here as an
    /// `Err`, never as a process failure.
    pub async fn run_turn(self: &Arc<Self>, session_id: &str) -> Result<TurnStats, TurnError> {
        let loaded = self
            .loader
            .get(&Aspect::Agent)
            .ok_or_else(|| TurnError::Reported("no agent component is loaded".to_string()))?;

        // The turn's stop signal is carried by the session's `CancelFlag`,
        // which host imports await directly; the budget only enforces the
        // grace window once one has been raised.
        let budget = Budget::new(format!("agent turn ({session_id})"), self.cfg.wasm_slice);
        let mut store = self.session_store(Caps::Agent, budget, session_id).await;

        let result = async {
            let agent = crate::bindings::agent::Agent::instantiate_async(
                &mut store,
                &loaded.component,
                self.runtime.linker(Caps::Agent),
            )
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating agent")?;

            agent
                .call_handle_turn(&mut store, session_id)
                .await
                .map_err(anyhow::Error::from)
                .context("calling handle-turn")
        }
        .await;

        match result {
            Ok(Ok(stats)) => Ok(stats),
            // The agent ran correctly and is telling us something went wrong.
            Ok(Err(msg)) => Err(TurnError::Reported(msg)),
            // The agent itself misbehaved: panic, blown budget, memory limit.
            Err(trap) => Err(TurnError::Trapped(format!("{trap:#}"))),
        }
    }

    /// Asks the agent which tools it would offer for this session's mode.
    ///
    /// The agent owns its tool surface, so this is a question rather than a
    /// guess — the panel can never drift from what the model actually sees.
    pub async fn agent_tools(self: &Arc<Self>, session_id: &str) -> Vec<ToolManifest> {
        // Only a worker can ask the agent anything; the gateway forwards.
        if let Role::Gateway(router) = &self.role {
            let out = crate::workers::call_session(
                self,
                router,
                session_id,
                "agent_tools",
                serde_json::json!({ "session": session_id }),
            )
            .await;
            return out
                .and_then(|v| Ok(serde_json::from_value(v)?))
                .unwrap_or_default();
        }

        let mode = self
            .persist
            .get_session(session_id)
            .await
            .ok()
            .flatten()
            .map(|m| m.mode)
            .unwrap_or_else(|| self.cfg.default_mode.clone());

        let Some(loaded) = self.loader.get(&Aspect::Agent) else {
            return Vec::new();
        };

        let budget = Budget::probe("agent list-tools", self.cfg.probe_budget);
        let mut store = self.session_store(Caps::Agent, budget, session_id).await;

        let result = async {
            let agent = crate::bindings::agent::Agent::instantiate_async(
                &mut store,
                &loaded.component,
                self.runtime.linker(Caps::Agent),
            )
            .await?;
            agent.call_list_tools(&mut store, &mode).await
        }
        .await;

        match result {
            Ok(tools) => tools,
            Err(e) => {
                tracing::warn!(error = %e, "agent could not list its tools");
                Vec::new()
            }
        }
    }

    // --- sessions ----------------------------------------------------------

    /// Routes a user message into a session, starting a turn or nudging one
    /// that is already running.
    pub async fn submit(
        self: &Arc<Self>,
        session_id: &str,
        message: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        if self.persist.get_session(session_id).await?.is_none() {
            return Err(anyhow!("no such session: {session_id}"));
        }
        if attachments.len() > self.cfg.max_attachments {
            return Err(anyhow!(
                "too many attachments: {} (limit {})",
                attachments.len(),
                self.cfg.max_attachments
            ));
        }
        for a in &attachments {
            // base64 inflates by 4/3; compare against the decoded size the
            // limit is expressed in.
            let decoded = a.data_base64.len() / 4 * 3;
            if decoded > self.cfg.max_attachment_bytes {
                return Err(anyhow!(
                    "attachment '{}' is {} bytes, over the {} byte limit",
                    a.name,
                    decoded,
                    self.cfg.max_attachment_bytes
                ));
            }
        }

        match &self.role {
            Role::Gateway(router) => {
                crate::workers::call_session(
                    self,
                    router,
                    session_id,
                    "submit",
                    serde_json::json!({
                        "session": session_id,
                        "message": message,
                        "attachments": attachments,
                    }),
                )
                .await?;
            }
            Role::Worker(_) => {
                self.sessions.submit(self, session_id, message, attachments);
            }
        }
        Ok(())
    }

    /// Stops the turn running for a session. Reports whether there was one.
    pub async fn cancel(self: &Arc<Self>, session_id: &str) -> bool {
        match &self.role {
            Role::Gateway(router) => {
                match crate::workers::call_session(
                    self,
                    router,
                    session_id,
                    "cancel",
                    serde_json::json!({ "session": session_id }),
                )
                .await
                {
                    Ok(v) => v
                        .get("stopped")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true),
                    Err(e) => {
                        tracing::warn!(session = %session_id, error = %e, "cancel did not reach the worker");
                        false
                    }
                }
            }
            Role::Worker(_) => self.sessions.cancel(session_id),
        }
    }

    /// The stop signal for a session, for host imports that must abandon a wait
    /// when the user presses stop.
    pub fn cancel_flag(&self, session_id: &str) -> Option<Arc<crate::session::CancelFlag>> {
        self.sessions.cancel_flag(session_id)
    }

    /// Picks an interrupted turn back up.
    pub async fn resume(self: &Arc<Self>, session_id: &str) {
        match &self.role {
            Role::Gateway(router) => {
                if let Err(e) = crate::workers::call_session(
                    self,
                    router,
                    session_id,
                    "resume",
                    serde_json::json!({ "session": session_id }),
                )
                .await
                {
                    tracing::warn!(session = %session_id, error = %e, "resume did not reach the worker");
                }
            }
            Role::Worker(_) => self.sessions.resume(self, session_id),
        }
    }

    // --- tools ---------------------------------------------------------------

    /// Records a tool's manifest, captured when the component passed validation.
    pub fn set_tool_manifest(&self, name: &str, manifest: ToolManifest) {
        if let Ok(mut map) = self.tool_manifests.write() {
            map.insert(name.to_string(), manifest);
        }
    }

    pub fn forget_tool(&self, name: &str) {
        if let Ok(mut map) = self.tool_manifests.write() {
            map.remove(name);
        }
    }

    /// Takes a aspect out of service, the mirror of [`install_component`].
    ///
    /// Used when a aspect's source is gone: without this the loader keeps serving
    /// the last artifact forever, so a deleted tool stays callable and every
    /// rebuild fails with "no crate found". Order matters — drop the manifest
    /// first, because `tool_registry` filters the manifest map by what the
    /// loader holds, and a reader between the two writes must see a tool that
    /// is missing rather than one that is loaded but undescribed.
    pub fn uninstall_component(&self, aspect: &Aspect) {
        if let Aspect::Tool(name) = aspect {
            self.forget_tool(name);
        }
        self.loader.remove(aspect);
    }

    /// Installs a component, keeping the tool registry in step with the loader.
    ///
    /// These two must never disagree: a tool that is loaded but unregistered is
    /// invisible to the model, which is indistinguishable from it not being
    /// installed at all. Routing every install through here is what makes that
    /// impossible to forget.
    pub async fn install_component(
        self: &Arc<Self>,
        component: Arc<crate::loader::LoadedComponent>,
    ) {
        let aspect = component.aspect.clone();
        self.loader.install(component);

        // Note: a worker's gateway build is deliberately NOT pushed to the
        // gateway process. Each conversation renders its own transcript with
        // its own build, but the page everyone loads is trunk's — a branch's
        // UI reaches other people only by merging.

        if let Aspect::Tool(name) = &aspect {
            match self.describe_tool(&aspect).await {
                Ok(manifest) => self.set_tool_manifest(name, manifest),
                Err(e) => {
                    // Loaded but unusable: say so rather than leaving a tool
                    // that silently never appears.
                    tracing::warn!(%aspect, error = %e, "tool loaded but its manifest could not be read");
                }
            }
        }
    }

    async fn describe_tool(self: &Arc<Self>, aspect: &Aspect) -> Result<ToolManifest> {
        let loaded = self
            .loader
            .get(aspect)
            .ok_or_else(|| anyhow!("{aspect} is not loaded"))?;

        let budget = Budget::probe(format!("{aspect} describe"), self.cfg.probe_budget);
        let mut store = self
            .runtime
            .new_store(self.clone(), Caps::Tool, budget, None);

        let tool = crate::bindings::tool::Tool::instantiate_async(
            &mut store,
            &loaded.component,
            self.runtime.linker(Caps::Tool),
        )
        .await
        .map_err(anyhow::Error::from)?;

        tool.call_describe(&mut store)
            .await
            .map_err(anyhow::Error::from)
            .context("describe")
    }

    /// Manifests for every tool currently loaded, in a stable order so the
    /// model's tool list does not churn between requests.
    pub fn tool_registry(&self) -> Vec<ToolManifest> {
        let Ok(map) = self.tool_manifests.read() else {
            return Vec::new();
        };
        let loaded: std::collections::HashSet<String> = self
            .loader
            .tools()
            .into_iter()
            .filter_map(|s| match s {
                Aspect::Tool(name) => Some(name),
                _ => None,
            })
            .collect();

        let mut out: Vec<ToolManifest> = map
            .iter()
            .filter(|(name, _)| loaded.contains(*name))
            .map(|(_, m)| m.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Runs one tool component in its own store.
    ///
    /// Failures come back as tool results rather than traps, so a broken tool
    /// interrupts a sentence, not the conversation.
    pub async fn invoke_tool(
        self: &Arc<Self>,
        name: &str,
        session_id: &str,
        args_json: &str,
    ) -> std::result::Result<String, String> {
        let aspect = Aspect::tool(name);
        let Some(loaded) = self.loader.get(&aspect) else {
            return Err(format!("no tool named '{name}' is loaded"));
        };

        // Scoped to this tool: it is handed its own settings and never sees
        // another's.
        let config_json = self.cfg.tool_config_json(name);

        let budget = Budget::new(format!("tool {name}"), self.cfg.tool_budget);
        let mut store = self.session_store(Caps::Tool, budget, session_id).await;

        let result = async {
            let tool = crate::bindings::tool::Tool::instantiate_async(
                &mut store,
                &loaded.component,
                self.runtime.linker(Caps::Tool),
            )
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating")?;

            tool.call_invoke(&mut store, session_id, args_json, &config_json)
                .await
                .map_err(anyhow::Error::from)
                .context("invoke")
        }
        .await;

        match result {
            Ok(Ok(output)) => Ok(self.truncate(output)),
            Ok(Err(message)) => Err(self.truncate(message)),
            Err(trap) => {
                // A trapping tool is a faulty revision; let the breaker see it.
                let detail = format!("{trap:#}");
                let grip = self.clone();
                let aspect = aspect.clone();
                let reported = detail.clone();
                tokio::spawn(async move {
                    crate::watchdog::report_failure(&grip, &aspect, &reported).await;
                });
                Err(format!("tool '{name}' crashed: {detail}"))
            }
        }
    }

    /// Caps anything headed for the model's context window.
    pub fn truncate(&self, text: String) -> String {
        let limit = self.cfg.max_tool_output_bytes;
        if text.len() <= limit {
            return text;
        }
        let mut cut = limit;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!(
            "{}\n\n[truncated: {} of {} bytes shown]",
            &text[..cut],
            cut,
            text.len()
        )
    }

    // --- watcher suppression ------------------------------------------------

    /// Tells the file watcher to ignore this aspect for a while, because the
    /// orchestrator is about to rewrite its source itself.
    pub fn suppress_watch(&self, aspect: &Aspect, window: std::time::Duration) {
        if let Ok(mut map) = self.watch_suppressed_until.lock() {
            map.insert(aspect.key(), std::time::Instant::now() + window);
        }
    }

    /// Suppresses the watcher for every aspect at once — a merge or reset
    /// rewrites files across the whole tree.
    pub fn suppress_watch_all(&self, window: std::time::Duration) {
        if let Ok(mut map) = self.watch_suppressed_until.lock() {
            map.insert("*".to_string(), std::time::Instant::now() + window);
        }
    }

    pub fn watch_suppressed(&self, aspect: &Aspect) -> bool {
        let Ok(mut map) = self.watch_suppressed_until.lock() else {
            return false;
        };
        if let Some(until) = map.get("*") {
            if *until > std::time::Instant::now() {
                return true;
            }
            map.remove("*");
        }
        match map.get(&aspect.key()) {
            Some(until) if *until > std::time::Instant::now() => true,
            Some(_) => {
                map.remove(&aspect.key());
                false
            }
            None => false,
        }
    }
}
