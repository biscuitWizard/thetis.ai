//! Implementations of every host import.
//!
//! This module is the entire attack surface a guest has against the system, so
//! each function validates its arguments, scopes access to the session the call
//! was made for, and caps the size of anything it hands back.

use wasmtime::Result;

/// Bridges the orchestrator's `anyhow` errors into wasmtime's error type.
/// A host error becomes a trap, which the caller catches per-call — it never
/// escalates into a process failure.
trait IntoWasmtime<T> {
    fn wt(self) -> Result<T>;
}

impl<T> IntoWasmtime<T> for anyhow::Result<T> {
    fn wt(self) -> Result<T> {
        self.map_err(wasmtime::Error::from_anyhow)
    }
}

fn err(msg: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(msg.into())
}

use crate::bindings::types::{
    Attachment, CompactionProgress, CompileReport, ConfigEntry, Dependency, EventRecord,
    ExecResult, FsEntry, InboxItem, LlmError, LogLevel, ModTarget, ModeInfo, ModelInfo,
    SessionEvent, SessionMeta, SshHostInfo, StreamChunk, TerminalInfo, TerminalOpen,
    TerminalOutput, ToolManifest,
};
use crate::bindings::{
    branch, configuration, control, delegation, devkit, hostfs, llm, sandbox, session, sys,
    terminal, tooling, transcripts,
};
use crate::grip::Grip;
use crate::runtime::HostState;
use std::sync::Arc;

/// Where a conversation's most recent LLM request is kept, for the inspector.
/// Read by the gateway straight out of the store, so it answers for stopped
/// and archived conversations too.
pub const LAST_REQUEST_KEY: &str = "__llm_request";

/// Ceiling on a captured request. Comfortably past any real context window,
/// and well under the 16 MiB websocket cap it has to travel through later.
const MAX_CAPTURED_REQUEST: usize = 8 * 1024 * 1024;

impl HostState {
    /// Rejects attempts to touch a session this call may not.
    ///
    /// An agent or tool store is pinned to one session and may touch only
    /// that. A gateway store is pinned to a *person*: it may touch any
    /// session that person owns (or everything, with `see_all_sessions`) and
    /// nothing else — the guest hands back whatever id the browser sent, so
    /// this is where a foreign `rename`, `archive`, `submit` or `events` is
    /// stopped. A store with neither, a probe or the renderer, is host
    /// business and unscoped.
    ///
    /// One rule for every call site on purpose: the first version kept the
    /// ownership check in a separate `may_access` and left the pre-existing
    /// `scope_ok` callers on the old "gateway is unscoped" rule, and a user
    /// could rename another's conversation by id.
    fn scope_ok(&self, session_id: &str) -> Result<()> {
        match &self.session_id {
            Some(mine) if mine != session_id => Err(err(format!(
                "session {session_id} is out of scope for this call (scoped to {mine})"
            ))),
            Some(_) => Ok(()),
            None => match &self.principal {
                Some(p) => crate::auth::may_access(self.grip(), p, session_id)
                    .map_err(|e| err(e.to_string())),
                None => Ok(()),
            },
        }
    }

    /// `scope_ok` by its ownership-flavoured name, for call sites that read
    /// better saying what they check.
    fn may_access(&self, session_id: &str) -> Result<()> {
        self.scope_ok(session_id)
    }
    fn require(&self, cap: crate::policy::Cap) -> Result<()> {
        if self.policy.denies(cap) {
            Err(err(format!("{cap:?} is withheld for this user by policy")))
        } else {
            Ok(())
        }
    }

    fn grip(&self) -> &Grip {
        &self.grip
    }

    /// Notes that this call changed a component, so the caller can tell the
    /// model when the change takes effect.
    fn note_pending_swap(&mut self, target: &ModTarget) {
        if let ModTarget::AgentSelf = target {
            if !self.pending_swaps.contains(&crate::aspect::Aspect::Agent) {
                self.pending_swaps.push(crate::aspect::Aspect::Agent);
            }
        }
    }

    /// This call's stop signal, if it is running on behalf of a session.
    fn cancel_flag(&self) -> Option<Arc<crate::session::CancelFlag>> {
        let session = self.session_id.as_deref()?;
        self.grip.cancel_flag(session)
    }

    /// Whether the user has already pressed stop for this call's session.
    fn cancelled(&self) -> bool {
        self.cancel_flag().is_some_and(|f| f.raised())
    }

    /// Runs a blocking host operation, abandoning it if the turn is stopped.
    ///
    /// This is what makes the stop button work during the calls that actually
    /// take time — a terminal command, a build, a model stream. Those await
    /// inside the host, where neither the guest's own inbox checkpoints nor the
    /// epoch deadline can reach them: epoch interruption only fires while the
    /// guest is executing wasm, so a guest parked in a host import was
    /// uninterruptible for as long as the import took. Racing the work against
    /// the stop signal is the only thing that can cut that short.
    ///
    /// The abandoned work is not killed, just stopped being waited for. That is
    /// deliberate: a half-written file or a half-finished build is worse than
    /// one that runs to completion with nobody listening. Terminal commands are
    /// the exception and are handled in [`crate::terminal`], which can leave the
    /// command running in its shell and return the output so far.
    ///
    /// Marks the budget on the way out, so a guest that ignores the returned
    /// error still cannot continue: its next wasm instruction traps.
    async fn interruptible<T>(
        &mut self,
        what: &str,
        work: impl std::future::Future<Output = T>,
    ) -> std::result::Result<T, String> {
        let Some(flag) = self.cancel_flag() else {
            // No session, so nothing to stop: a probe or a gateway call.
            return Ok(work.await);
        };
        // Already stopped before the work began — do not start it at all.
        if flag.raised() {
            self.budget.cancel();
            return Err(stopped_message(what));
        }

        let outcome = tokio::select! {
            // Biased so that a stop raised at the same moment the work
            // finishes does not discard a result that is already in hand.
            biased;
            done = work => Ok(done),
            () = flag.cancelled() => Err(stopped_message(what)),
        };

        if outcome.is_err() {
            tracing::info!(what, "abandoning a host call: the turn was stopped");
            self.budget.cancel();
        }
        outcome
    }
}

/// What the guest is told when its call was cut short by the user.
fn stopped_message(what: &str) -> String {
    format!("{what} was interrupted: you stopped this turn")
}

// --- sys -------------------------------------------------------------------

impl sys::Host for HostState {
    async fn log(&mut self, level: LogLevel, msg: String) -> Result<()> {
        self.budget.entered_host("log");
        let msg = msg.chars().take(4096).collect::<String>();
        match level {
            LogLevel::Trace => tracing::trace!(target: "guest", "{msg}"),
            LogLevel::Debug => tracing::debug!(target: "guest", "{msg}"),
            LogLevel::Info => tracing::info!(target: "guest", "{msg}"),
            LogLevel::Warn => tracing::warn!(target: "guest", "{msg}"),
            LogLevel::Error => tracing::error!(target: "guest", "{msg}"),
        }
        Ok(())
    }

    async fn now_ms(&mut self) -> Result<u64> {
        self.budget.entered_host("now_ms");
        Ok(crate::store::now_ms())
    }

    async fn kv_get(&mut self, scope: String, key: String) -> Result<Option<String>> {
        self.budget.entered_host("kv_get");
        let scope = if scope == "user" {
            if let Some(p) = &self.principal {
                format!("user:{}", p.user_id)
            } else if let Some(id) = &self.session_id {
                format!(
                    "user:{}",
                    self.grip()
                        .persist
                        .owner_of_root(id)
                        .await
                        .wt()?
                        .unwrap_or_else(|| "local".into())
                )
            } else {
                return Err(err("no user for this call"));
            }
        } else if scope.starts_with("user:") {
            return Err(err("user scopes are addressed as `user`"));
        } else {
            scope
        };
        if scope != "global" && !scope.starts_with("user:") {
            self.may_access(&scope)?;
        }
        self.grip().persist.kv_get(&scope, &key).await.wt()
    }

    async fn kv_put(&mut self, scope: String, key: String, value: String) -> Result<()> {
        self.budget.entered_host("kv_put");
        let scope = if scope == "user" {
            if let Some(p) = &self.principal {
                format!("user:{}", p.user_id)
            } else if let Some(id) = &self.session_id {
                format!(
                    "user:{}",
                    self.grip()
                        .persist
                        .owner_of_root(id)
                        .await
                        .wt()?
                        .unwrap_or_else(|| "local".into())
                )
            } else {
                return Err(err("no user for this call"));
            }
        } else if scope.starts_with("user:") {
            return Err(err("user scopes are addressed as `user`"));
        } else {
            scope
        };
        if scope != "global" && !scope.starts_with("user:") {
            self.may_access(&scope)?;
        }
        if scope == "global"
            && self.session_id.is_none()
            && self.principal.as_ref().is_some_and(|p| !p.is_admin())
        {
            return Err(err("only an administrator may write global settings"));
        }
        if value.len() > 1 << 20 {
            return Err(err("kv value exceeds 1 MiB"));
        }
        self.grip()
            .persist
            .kv_put(&scope, &key, &value)
            .await
            .wt()?;
        Ok(())
    }

    /// Non-secret configuration. Secrets are deliberately unreachable: the
    /// OpenRouter key never crosses this boundary.
    async fn config_get(&mut self, key: String) -> Result<Option<String>> {
        self.budget.entered_host("config_get");
        let cfg = &self.grip().cfg;
        Ok(match key.as_str() {
            "model" => Some(self.policy.default_model.clone()),
            "policy_read_only" => Some(self.policy.read_only.to_string()),
            "policy_deny_tools" => Some(self.policy.deny_tools.join(",")),
            "policy_deny_groups" => Some(self.policy.deny_groups.join(",")),
            "policy_models_restricted" => Some(self.policy.models_restricted.to_string()),
            // What the agent calls itself. The harness is always Thetis; this
            // is the name the agent answers to in a prompt or on screen.
            "agent_name" => Some(cfg.agent_name.clone()),
            // Image URL or data: URI for the agent's avatar; empty means the
            // gateway draws its built-in mark.
            "agent_avatar" => Some(cfg.agent_avatar.clone()),
            "system_prompt" => Some(cfg.system_prompt.clone()),
            "max_iterations" => Some(cfg.max_iterations.to_string()),
            "max_tool_output_bytes" => Some(cfg.max_tool_output_bytes.to_string()),
            "sandbox_available" => Some(cfg.sandbox_available.to_string()),
            // Where the shared workspace really is on this machine. A guest
            // reaches it as the preopen `/workspace` and cannot learn the host
            // path from the preopen itself, but it needs it to say anything
            // useful in a terminal command — the filesystem tools are rooted at
            // the conversation's own checkout, which the workspace sits outside
            // of. Not a secret: it is printed in the UI's workspace explorer.
            "workspace_dir" => cfg.wasi.dirs.first().map(|d| d.display().to_string()),
            // The dev kit is wired up; the agent uses this to decide whether to
            // offer itself the self-modification tools.
            "devkit_available" => Some(
                (cfg.devkit.enabled && !self.policy.denies(crate::policy::Cap::Devkit)).to_string(),
            ),
            // Context compaction. The agent owns the decision of what to shed,
            // so it needs the thresholds rather than being told when to act.
            "compact_enabled" => Some(cfg.context.enabled.to_string()),
            "context_window" => Some(cfg.context.window.to_string()),
            "compact_threshold" => Some(cfg.context.compact_threshold.to_string()),
            "compact_target" => Some(cfg.context.compact_target.to_string()),
            "summary_model" => Some(if cfg.context.summary_model.is_empty() {
                cfg.model.clone()
            } else {
                cfg.context.summary_model.clone()
            }),
            "keep_head" => Some(cfg.context.keep_head.to_string()),
            "keep_tail" => Some(cfg.context.keep_tail.to_string()),
            // Tool-surface scoping. The agent owns which groups it offers
            // itself, so it reads the policy rather than being handed a list.
            "tool_grouping_enabled" => Some(cfg.tool_groups.grouping_enabled.to_string()),
            "tool_accounting_enabled" => Some(cfg.tool_groups.accounting_enabled.to_string()),
            "tool_groups_always_on" => Some(cfg.tool_groups.always_on.join(",")),
            "tool_route_threshold" => Some(cfg.tool_groups.route_threshold.to_string()),
            _ => None,
        })
    }

    async fn list_models(&mut self) -> Result<Vec<ModelInfo>> {
        self.budget.entered_host("list_models");
        Ok(self
            .grip()
            .cfg
            .models
            .iter()
            .filter(|m| self.policy.allows_model(&m.id))
            .map(|m| ModelInfo {
                id: m.id.clone(),
                label: m.label.clone(),
            })
            .collect())
    }

    async fn list_modes(&mut self) -> Result<Vec<ModeInfo>> {
        self.budget.entered_host("list_modes");
        Ok(self
            .grip()
            .cfg
            .modes
            .iter()
            .filter(|m| self.policy.allows_mode(&m.id))
            .map(|m| ModeInfo {
                id: m.id.clone(),
                label: m.label.clone(),
                description: m.description.clone(),
                read_only: self.policy.read_only || m.read_only,
                prompt: m.prompt.clone(),
            })
            .collect())
    }
}

// --- session ---------------------------------------------------------------

impl session::Host for HostState {
    async fn events(&mut self, session_id: String, from_seq: u64) -> Result<Vec<EventRecord>> {
        self.budget.entered_host("events");
        self.scope_ok(&session_id)?;
        self.grip().persist.events(&session_id, from_seq).await.wt()
    }

    async fn append(&mut self, session_id: String, event: SessionEvent) -> Result<u64> {
        self.budget.entered_host("append");
        self.scope_ok(&session_id)?;
        self.grip().append_event(&session_id, event).await.wt()
    }

    async fn emit_output(&mut self, session_id: String, chunk: String) -> Result<()> {
        self.budget.entered_host("emit_output");
        self.scope_ok(&session_id)?;
        self.grip()
            .publish_transient(&session_id, SessionEvent::StreamDelta(chunk));
        Ok(())
    }

    /// A reasoning fragment, transient like a token delta.
    ///
    /// Separate from `emit_output` because reasoning is not the answer: a
    /// local DeepSeek-style model can spend forty chunks thinking before two
    /// chunks of reply, and splicing that into the transcript would corrupt
    /// the assistant message. The surface shows it as its own collapsible
    /// element, and nothing persists it — only the answer is durable.
    async fn emit_reasoning(&mut self, session_id: String, chunk: String) -> Result<()> {
        self.budget.entered_host("emit_reasoning");
        self.scope_ok(&session_id)?;
        self.grip()
            .publish_transient(&session_id, SessionEvent::ReasoningDelta(chunk));
        Ok(())
    }

    /// Compaction progress, transient like a token delta.
    ///
    /// Not persisted: the log already records the outcome as
    /// `context-compacted`, and how far along a summary run got is only
    /// interesting while it is running. What it buys is a surface that shows
    /// something during the tens of seconds compaction can take, instead of a
    /// started turn that says nothing.
    async fn emit_compaction_progress(
        &mut self,
        session_id: String,
        progress: CompactionProgress,
    ) -> Result<()> {
        self.budget.entered_host("emit_compaction_progress");
        self.scope_ok(&session_id)?;
        self.grip()
            .publish_transient(&session_id, SessionEvent::CompactionProgress(progress));
        Ok(())
    }

    async fn poll_inbox(&mut self, session_id: String) -> Result<Vec<InboxItem>> {
        self.budget.entered_host("poll_inbox");
        self.scope_ok(&session_id)?;
        let mut items = self.grip().sessions.drain_inbox(&session_id);

        // The flag, not the queue, is the authority on whether the turn was
        // stopped. A queue item can be consumed by an earlier poll and then
        // forgotten — the guest that read it might discard it, or drain the
        // inbox at a checkpoint it does not act on — whereas the flag stays
        // raised for the rest of the turn. Synthesizing the item here means
        // every poll after a stop reports it, however the guest is written.
        if self.cancelled() && !items.iter().any(|i| matches!(i, InboxItem::Cancel)) {
            items.push(InboxItem::Cancel);
        }

        // A cancel must also stop a guest that ignores the item, so arm the
        // budget: the next wasm instruction after this call then traps.
        if items.iter().any(|i| matches!(i, InboxItem::Cancel)) {
            self.budget.cancel();
        }
        Ok(items)
    }

    async fn list_sessions(&mut self, include_archived: bool) -> Result<Vec<SessionMeta>> {
        self.budget.entered_host("list_sessions");
        // Whatever model a session names is reported as-is. The catalogue is
        // what the picker offers, not the set of ids the provider accepts, so
        // silently swapping an unlisted one for the default made a deliberate
        // choice look like it had been ignored.
        //
        // Whose conversations: the principal's own, unless this connection has
        // asked for everyone's and the policy lets it (`Principal::list_owner`).
        // An agent store lists its owner's; a store with neither — a
        // local-mode probe — lists all.
        let owned = if let Some(p) = &self.principal {
            p.list_owner().map(str::to_string)
        } else if let Some(id) = &self.session_id {
            self.grip().persist.owner_of_root(id).await.wt()?
        } else {
            None
        };
        self.grip()
            .persist
            .list_sessions_owned(owned.as_deref(), include_archived)
            .await
            .wt()
    }

    async fn get_session(&mut self, session_id: String) -> Result<Option<SessionMeta>> {
        self.budget.entered_host("get_session");
        self.may_access(&session_id)?;
        self.grip().persist.get_session(&session_id).await.wt()
    }

    async fn create_session(&mut self, title: Option<String>) -> Result<String> {
        self.budget.entered_host("create_session");
        // Creating conversations is a gateway's job. An agent turn asking for
        // one is the first half of a delegation bypass: a session minted this
        // way is in nobody's sub-agent registry, so it has no parent, does not
        // count against the fan-out cap, never settles a result back, is not
        // hidden from the sidebar, and — because the one-level rule is decided
        // by registry membership — could itself spawn children. `submit` below
        // refuses to drive somebody else's session, which breaks the second
        // half, but a capability that has no legitimate caller is better
        // refused outright than left as half an exploit.
        if self.session_id.is_some() {
            return Err(err(
                "an agent cannot create conversations; use spawn_agent to delegate",
            ));
        }
        let mode = self.policy.default_mode.clone();
        let owner = self
            .principal
            .as_ref()
            .map(|p| p.user_id.as_str())
            .unwrap_or("local");
        self.grip()
            .persist
            .create_session(title, &mode, owner)
            .await
            .map(|s| s.id)
            .wt()
    }

    async fn rename_session(&mut self, session_id: String, title: String) -> Result<()> {
        self.budget.entered_host("rename_session");
        self.scope_ok(&session_id)?;
        self.grip()
            .persist
            .rename_session(&session_id, &title)
            .await
            .wt()?;
        Ok(())
    }

    async fn archive_session(&mut self, session_id: String, archived: bool) -> Result<()> {
        self.budget.entered_host("archive_session");
        self.scope_ok(&session_id)?;
        self.grip()
            .persist
            .archive_session(&session_id, archived)
            .await
            .wt()?;
        // An archived conversation's worker has nothing left to do, and its
        // checkout is disposable — the branch and every commit stay. All
        // gateway-side: the registry and the fleet live there.
        if archived {
            if let crate::grip::Role::Gateway(router) = &self.grip.role {
                let grip = self.grip.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    if let Some(peer) = router.live_peer(&session_id).await {
                        router.mark_stopping(&session_id).await;
                        let _ = peer.call("shutdown", serde_json::Value::Null).await;
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    let Some(store) = grip.local_store() else {
                        return;
                    };
                    let branches = crate::branches::Branches::new(grip.cfg.clone(), store.clone());
                    if let Ok(Some(mut row)) = branches.get(&session_id) {
                        row.state = crate::branches::BranchState::Archived;
                        let _ = branches.update(&row);
                        if let Err(e) = branches.release_worktree(&session_id).await {
                            tracing::warn!(session = %session_id, error = %e,
                                "could not release the archived checkout");
                        }
                    }
                });
            }
        }
        self.yielded();
        Ok(())
    }

    async fn submit(
        &mut self,
        session_id: String,
        message: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        self.budget.entered_host("submit");
        // Scoped like `append` and `events`: a turn may only drive its own
        // session. Without this an agent could start turns in any conversation,
        // which is both a way round the sub-agent registry and a way to talk
        // into somebody else's chat. Delegation is the sanctioned path, and it
        // routes through `spawn`, which registers what it starts.
        self.scope_ok(&session_id)?;
        let grip = self.grip.clone();
        grip.submit(&session_id, message, attachments).await.wt()?;
        // Submitting can materialize a branch worker — worktree, spawn, aspect
        // loading — which is host time, not guest spinning.
        self.yielded();
        Ok(())
    }

    async fn set_session_mode(&mut self, session_id: String, mode: String) -> Result<()> {
        self.budget.entered_host("set_session_mode");
        self.may_access(&session_id)?;
        if !self.policy.allows_mode(&mode) {
            return Err(err(format!("mode `{mode}` is not available to this user")));
        };
        // Only offered modes are accepted, so a guest cannot invent one the
        // agent has no handling for.
        let known = self.grip().cfg.mode(&mode).is_some();
        if !known {
            return Err(err(format!("unknown mode: {mode}")));
        }
        self.grip()
            .persist
            .set_mode(&session_id, &mode)
            .await
            .wt()?;
        Ok(())
    }

    async fn available_tools(&mut self, session_id: String) -> Result<Vec<ToolManifest>> {
        self.budget.entered_host("available_tools");
        self.may_access(&session_id)?;
        let grip = self.grip.clone();
        let tools = grip.agent_tools(&session_id).await;
        self.yielded();
        Ok(tools)
    }

    async fn set_session_model(&mut self, session_id: String, model: String) -> Result<()> {
        self.budget.entered_host("set_session_model");
        self.may_access(&session_id)?;
        // Only bites when a role or user has actually narrowed the list; see
        // `EffectivePolicy::allows_model` for why an unrestricted user is not
        // held to the configured catalogue.
        if !model.is_empty() && !self.policy.allows_model(&model) {
            return Err(err(format!(
                "model `{model}` is not available to this user"
            )));
        };
        self.grip()
            .persist
            .set_model(&session_id, &model)
            .await
            .wt()?;
        Ok(())
    }
}

// --- llm -------------------------------------------------------------------

impl HostState {
    fn requested_model(request_json: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(request_json)
            .ok()?
            .get("model")?
            .as_str()
            .map(str::to_string)
    }

    fn check_model(&self, request_json: &str) -> std::result::Result<(), LlmError> {
        let Some(model) = Self::requested_model(request_json) else {
            return Ok(());
        };
        if self.policy.allows_model(&model) {
            Ok(())
        } else {
            Err(LlmError::BadRequest(format!(
                "model `{model}` is not available to this user; pick one of: {}",
                self.policy.models.join(", ")
            )))
        }
    }

    /// Refuses a call that would push the session past its spend ceiling.
    ///
    /// Takes owned values rather than `&self`: holding the host state across
    /// the persistence await would make every LLM future non-Send.
    async fn check_budget(
        persist: crate::persist::Persist,
        session_id: Option<String>,
        session_limit: f64,
        owner: Option<String>,
        user_limit: f64,
    ) -> std::result::Result<(), LlmError> {
        if session_limit > 0.0 {
            if let Some(sid) = session_id {
                let spent = persist.get_spend(&sid).await.unwrap_or(0.0);
                if spent >= session_limit {
                    return Err(LlmError::Budget(format!(
                        "session has spent ${spent:.4} of its ${session_limit:.4} limit"
                    )));
                }
            }
        }
        if user_limit > 0.0 {
            if let Some(owner) = owner {
                let spent = persist.get_user_spend(&owner).await.unwrap_or(0.0);
                if spent >= user_limit {
                    return Err(LlmError::Budget(format!(
                        "user has spent ${spent:.4} of their ${user_limit:.4} limit"
                    )));
                }
            }
        }
        Ok(())
    }

    fn budget_inputs(
        &self,
    ) -> (
        crate::persist::Persist,
        Option<String>,
        f64,
        Option<String>,
        f64,
    ) {
        (
            self.grip.persist.clone(),
            self.session_id.clone(),
            self.grip.cfg.session_spend_limit_usd,
            self.principal.as_ref().map(|p| p.user_id.clone()),
            self.policy.spend_limit_usd,
        )
    }

    /// Keeps the request the provider just received, for the web UI's
    /// inspector to show the conversation as the model sees it.
    ///
    /// In the store rather than in this process's memory, because a worker is
    /// the shortest-lived thing here — it is reaped when idle, restarts onto
    /// kernels it built, and dies with a bad build. A capture that vanished
    /// with it would leave the inspector empty exactly when someone went
    /// looking. One entry per conversation: the newest request answers "what
    /// does my prompt look like", and keeping every turn's would grow the
    /// database by a context window per turn.
    fn capture_request(&self, llm: &Arc<crate::llm::LlmClient>) {
        let (Some(sid), Some(captured)) = (self.session_id.clone(), llm.last_request()) else {
            return;
        };
        let envelope = serde_json::json!({ "ts_ms": captured.ts_ms, "body": &*captured.body });
        let Ok(text) = serde_json::to_string(&envelope) else {
            return;
        };
        // A context this large cannot survive compaction, so it means something
        // is wrong upstream; storing it would be the second problem.
        if text.len() > MAX_CAPTURED_REQUEST {
            tracing::warn!(
                bytes = text.len(),
                "request too large to capture for the inspector"
            );
            return;
        }
        let persist = self.grip.persist.clone();
        tokio::spawn(async move {
            if let Err(e) = persist.kv_put(&sid, LAST_REQUEST_KEY, &text).await {
                tracing::debug!(error = %e, "request capture was not stored");
            }
        });
    }

    fn record_usage(&self, chunk: &StreamChunk) {
        let (StreamChunk::Finished(info), Some(sid)) = (chunk, &self.session_id) else {
            return;
        };
        if let Some(usage) = &info.usage {
            if usage.cost_usd > 0.0 {
                let persist = self.grip.persist.clone();
                let sid = sid.clone();
                let cost = usage.cost_usd;
                let principal_owner = self.principal.as_ref().map(|p| p.user_id.clone());
                tokio::spawn(async move {
                    if let Err(e) = persist.add_spend(&sid, cost).await {
                        tracing::warn!(error = %e, "spend was not recorded");
                    }
                    let owner = match principal_owner {
                        Some(owner) => Some(owner),
                        None => persist.owner_of_root(&sid).await.ok().flatten(),
                    };
                    if let Some(owner) = owner {
                        if let Err(e) = persist.add_user_spend(&owner, cost).await {
                            tracing::warn!(error = %e, "user spend was not recorded");
                        }
                    }
                });
            }
        }
    }
}

impl llm::Host for HostState {
    async fn chat(
        &mut self,
        request_json: String,
    ) -> Result<std::result::Result<String, LlmError>> {
        self.budget.entered_host("chat");
        if let Err(e) = self.check_model(&request_json) {
            return Ok(Err(e));
        }
        let (persist, sid, session_limit, owner, user_limit) = self.budget_inputs();
        if let Err(e) = Self::check_budget(persist, sid, session_limit, owner, user_limit).await {
            return Ok(Err(e));
        }
        let llm = self.grip.llm.clone();
        // Interruptible, unlike `stream_next`'s own hand-rolled race, because
        // this is a single await that can last tens of seconds and had no stop
        // checkpoint at all. Compaction is the caller that made this matter:
        // it runs several of these back to back before the turn's first
        // completion, and while they ran the stop button, the terminal views
        // and everything else driven by the turn were dead.
        let session = self.session_id.clone();
        let result = self
            .interruptible(
                "the completion",
                llm.chat_for(&request_json, session.as_deref()),
            )
            .await;
        self.yielded();
        Ok(match result {
            Ok(result) => result,
            Err(stopped) => Err(LlmError::Transport(stopped)),
        })
    }

    async fn stream_open(
        &mut self,
        request_json: String,
    ) -> Result<std::result::Result<u64, LlmError>> {
        self.budget.entered_host("stream_open");
        if let Err(e) = self.check_model(&request_json) {
            return Ok(Err(e));
        }
        let (persist, sid, session_limit, owner, user_limit) = self.budget_inputs();
        if let Err(e) = Self::check_budget(persist, sid, session_limit, owner, user_limit).await {
            return Ok(Err(e));
        }
        let llm = self.grip.llm.clone();
        let opened = llm
            .open_stream_for(&request_json, self.session_id.as_deref())
            .await;
        self.yielded();
        self.capture_request(&llm);

        match opened {
            Ok(handle) => {
                let id = self.next_stream_id;
                self.next_stream_id += 1;
                self.streams.insert(id, handle);
                Ok(Ok(id))
            }
            Err(e) => Ok(Err(e)),
        }
    }

    async fn stream_next(
        &mut self,
        stream_id: u64,
    ) -> Result<std::result::Result<StreamChunk, LlmError>> {
        self.budget.entered_host("stream_next");
        // Checked before awaiting the next chunk rather than only after: a
        // model that is still talking would otherwise stream a whole answer
        // into a turn the user has already stopped.
        if self.cancelled() {
            self.budget.cancel();
            self.streams.remove(&stream_id);
            return Ok(Err(LlmError::Transport(stopped_message("the completion"))));
        }

        let flag = self.cancel_flag();
        let chunk = {
            let Some(handle) = self.streams.get_mut(&stream_id) else {
                return Ok(Err(LlmError::BadRequest(format!(
                    "unknown stream id {stream_id}"
                ))));
            };
            match flag {
                Some(flag) => {
                    tokio::select! {
                        biased;
                        chunk = handle.next() => chunk,
                        () = flag.cancelled() => {
                            Err(LlmError::Transport(stopped_message("the completion")))
                        }
                    }
                }
                None => handle.next().await,
            }
        };
        // Time spent waiting on the model is not the guest spinning.
        self.yielded();

        if self.cancelled() {
            // Dropping the handle closes the upstream HTTP response, so the
            // provider stops generating tokens nobody will read.
            self.streams.remove(&stream_id);
            self.budget.cancel();
        }

        if let Ok(chunk) = &chunk {
            self.record_usage(chunk);
        }
        Ok(chunk)
    }

    async fn stream_close(&mut self, stream_id: u64) -> Result<()> {
        self.budget.entered_host("stream_close");
        self.streams.remove(&stream_id);
        Ok(())
    }
}

// --- sandbox (M3) ----------------------------------------------------------

const SANDBOX_UNAVAILABLE: &str =
    "the docker exec sandbox is not configured; code execution is unavailable";

impl sandbox::Host for HostState {
    async fn exec(
        &mut self,
        _session_id: String,
        _command: String,
        _stdin: Option<String>,
        _timeout_ms: u32,
    ) -> Result<ExecResult> {
        self.budget.entered_host("exec");
        Ok(ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: SANDBOX_UNAVAILABLE.to_string(),
            timed_out: false,
            truncated: false,
        })
    }

    async fn write_file(
        &mut self,
        _session_id: String,
        _path: String,
        _contents: String,
    ) -> Result<std::result::Result<(), String>> {
        self.budget.entered_host("write_file");
        Ok(Err(SANDBOX_UNAVAILABLE.to_string()))
    }

    async fn read_file(
        &mut self,
        _session_id: String,
        _path: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("read_file");
        Ok(Err(SANDBOX_UNAVAILABLE.to_string()))
    }

    async fn list_files(
        &mut self,
        _session_id: String,
        _path: String,
    ) -> Result<std::result::Result<Vec<String>, String>> {
        self.budget.entered_host("list_files");
        Ok(Err(SANDBOX_UNAVAILABLE.to_string()))
    }

    async fn available(&mut self) -> Result<bool> {
        self.budget.entered_host("available");
        Ok(false)
    }
}

// --- tooling (M3) ----------------------------------------------------------

impl tooling::Host for HostState {
    async fn registry(&mut self) -> Result<Vec<ToolManifest>> {
        self.budget.entered_host("registry");
        Ok(self.grip().tool_registry())
    }

    async fn invoke(
        &mut self,
        name: String,
        session_id: String,
        args_json: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("invoke");
        self.require(crate::policy::Cap::ComponentTools)?;
        self.may_access(&session_id)?;
        if self.policy.denies_tool(&name) {
            return Ok(Err(format!("'{name}' is withheld for this user by policy")));
        }
        if let Some(group) = self
            .grip()
            .tool_registry()
            .into_iter()
            .find(|manifest| manifest.name == name)
            .and_then(|manifest| {
                manifest
                    .capabilities
                    .into_iter()
                    .find_map(|cap| cap.strip_prefix("group:").map(str::to_string))
            })
        {
            if self.policy.denies_group(&group) {
                return Ok(Err(format!(
                    "'{name}' belongs to tool group '{group}', which is withheld by policy"
                )));
            }
        }
        if self.policy.read_only {
            let read_only = self
                .grip()
                .tool_registry()
                .into_iter()
                .find(|manifest| manifest.name == name)
                .is_some_and(|manifest| manifest.capabilities.iter().any(|c| c == "read-only"));
            if !read_only {
                return Ok(Err(format!(
                    "'{name}' may change something, and this user is read-only"
                )));
            }
        }
        let grip = self.grip.clone();
        // A tool can spend a long time on the network. Stopping the turn should
        // not mean waiting for it, and certainly should not mean the remaining
        // tools in the batch run afterwards.
        let result = self
            .interruptible(
                &format!("the tool '{name}'"),
                grip.invoke_tool(&name, &session_id, &args_json),
            )
            .await;
        // Running a tool can take real time; that is not the agent spinning.
        self.yielded();
        Ok(match result {
            Ok(inner) => inner,
            Err(stopped) => Err(stopped),
        })
    }

    async fn mcp_list_tools(&mut self) -> Result<Vec<ToolManifest>> {
        self.budget.entered_host("mcp_list_tools");
        Ok(Vec::new())
    }

    async fn mcp_call_tool(
        &mut self,
        name: String,
        _args_json: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("mcp_call_tool");
        Ok(Err(format!("no mcp server provides {name}")))
    }
}

// --- devkit ----------------------------------------------------------------

impl devkit::Host for HostState {
    async fn new_tool(&mut self, name: String, description: String) -> Result<CompileReport> {
        self.budget.entered_host("new_tool");
        self.require(crate::policy::Cap::Devkit)?;
        let grip = self.grip.clone();
        let report = crate::devkit::new_tool(&grip, &name, &description).await;
        // Compiling is slow by nature; do not charge it to the guest's budget.
        self.yielded();
        Ok(report)
    }

    async fn write_file(
        &mut self,
        target: ModTarget,
        path: String,
        contents: String,
    ) -> Result<CompileReport> {
        self.budget.entered_host("write_file");
        self.require(crate::policy::Cap::Devkit)?;
        let grip = self.grip.clone();
        let report = crate::devkit::write_file(&grip, &target, &path, &contents).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn patch_file(
        &mut self,
        target: ModTarget,
        path: String,
        old_text: String,
        new_text: String,
    ) -> Result<CompileReport> {
        self.budget.entered_host("patch_file");
        self.require(crate::policy::Cap::Devkit)?;
        let grip = self.grip.clone();
        let report = crate::devkit::patch_file(&grip, &target, &path, &old_text, &new_text).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn add_dependency(
        &mut self,
        target: ModTarget,
        dep: Dependency,
    ) -> Result<CompileReport> {
        self.budget.entered_host("add_dependency");
        self.require(crate::policy::Cap::Devkit)?;
        let grip = self.grip.clone();
        let dep = crate::manifest::Dependency {
            name: dep.name,
            version: dep.version,
            features: dep.features,
            default_features: dep.default_features,
        };
        let report = crate::devkit::add_dependency(&grip, &target, &dep).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn remove_dependency(
        &mut self,
        target: ModTarget,
        name: String,
    ) -> Result<CompileReport> {
        self.budget.entered_host("remove_dependency");
        self.require(crate::policy::Cap::Devkit)?;
        let grip = self.grip.clone();
        let report = crate::devkit::remove_dependency(&grip, &target, &name).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn list_dependencies(
        &mut self,
        target: ModTarget,
    ) -> Result<std::result::Result<Vec<Dependency>, String>> {
        self.budget.entered_host("list_dependencies");
        let grip = self.grip.clone();
        Ok(
            crate::devkit::list_dependencies(&grip, &target).map(|deps| {
                deps.into_iter()
                    .map(|d| Dependency {
                        name: d.name,
                        version: d.version,
                        features: d.features,
                        default_features: d.default_features,
                    })
                    .collect()
            }),
        )
    }

    async fn read_file(
        &mut self,
        target: ModTarget,
        path: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("read_file");
        let grip = self.grip.clone();
        Ok(crate::devkit::read_file(&grip, &target, &path).map(|text| grip.truncate(text)))
    }

    async fn list_files(
        &mut self,
        target: ModTarget,
    ) -> Result<std::result::Result<Vec<String>, String>> {
        self.budget.entered_host("list_files");
        let grip = self.grip.clone();
        Ok(crate::devkit::list_files(&grip, &target))
    }
}

// --- branch ------------------------------------------------------------------

impl HostState {
    /// Branch operations act on the conversation this turn belongs to; a
    /// probe context has no conversation and gets a clear refusal.
    fn branch_session(&self) -> Result<String> {
        self.session_id
            .clone()
            .ok_or_else(|| err("branch operations need a session"))
    }
}

impl branch::Host for HostState {
    async fn status(&mut self) -> Result<std::result::Result<branch::BranchState, String>> {
        self.budget.entered_host("status");
        let grip = self.grip.clone();
        let out = crate::branchops::status(&grip)
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn log(&mut self, limit: u32) -> Result<Vec<branch::CommitInfo>> {
        self.budget.entered_host("log");
        let grip = self.grip.clone();
        let out = crate::branchops::log(&grip, limit)
            .await
            .unwrap_or_default();
        self.yielded();
        Ok(out)
    }

    async fn update_from_trunk(
        &mut self,
    ) -> Result<std::result::Result<branch::BranchState, String>> {
        self.budget.entered_host("update_from_trunk");
        self.require(crate::policy::Cap::BranchWrite)?;
        let session = self.branch_session()?;
        let grip = self.grip.clone();
        let out = crate::branchops::update_from_trunk(&grip, &session)
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn reset_to(
        &mut self,
        rev: String,
    ) -> Result<std::result::Result<branch::BranchState, String>> {
        self.budget.entered_host("reset_to");
        self.require(crate::policy::Cap::BranchWrite)?;
        let session = self.branch_session()?;
        let grip = self.grip.clone();
        let out = crate::branchops::reset_to(&grip, &session, &rev)
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn complete_merge(
        &mut self,
        message: Option<String>,
    ) -> Result<std::result::Result<branch::BranchState, String>> {
        self.budget.entered_host("complete_merge");
        self.require(crate::policy::Cap::BranchWrite)?;
        let session = self.branch_session()?;
        let grip = self.grip.clone();
        let out = crate::branchops::complete_merge(&grip, &session, message)
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn abort_merge(&mut self) -> Result<std::result::Result<branch::BranchState, String>> {
        self.budget.entered_host("abort_merge");
        self.require(crate::policy::Cap::BranchWrite)?;
        let session = self.branch_session()?;
        let grip = self.grip.clone();
        let out = crate::branchops::abort_merge(&grip, &session)
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }
}

// --- delegation --------------------------------------------------------------
//
// The agent's view of sub-agents. Every function here is scoped to the session
// whose turn is running: a guest cannot spawn a child for somebody else, cannot
// read another conversation's children, and cannot cancel a child that is not
// its own. `delegation::spawn` and `cancel_child` enforce the last two; this
// layer's job is to refuse a call that has no session at all, and to keep what
// crosses back into the guest small.

impl HostState {
    /// Delegation acts on behalf of the conversation this turn belongs to. A
    /// probe context has none, and a tool-listing probe must not be able to
    /// spawn anything.
    fn delegating_session(&self) -> Result<String> {
        self.session_id
            .clone()
            .ok_or_else(|| err("delegation needs a session"))
    }
}

impl delegation::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        self.budget.entered_host("available");
        let cfg = &self.grip().cfg;
        if !cfg.subagents.enabled || self.policy.denies(crate::policy::Cap::Delegation) {
            return Ok(false);
        }
        // A sub-agent is told delegation is unavailable, not merely refused it
        // at dispatch. Both are enforced, but this is the one that keeps the
        // tools out of its prompt in the first place — the cheaper refusal,
        // because a capability never offered is never attempted.
        //
        // With no session this is a probe — the chat surface asking what the
        // tool surface looks like — and the honest answer is the configured
        // one. Answering `false` here would hide delegation from the tool panel
        // of a deployment that has it switched on.
        let Some(session) = self.session_id.clone() else {
            return Ok(true);
        };
        let grip = self.grip.clone();
        let is_child = grip
            .persist
            .get_subagent(&session)
            .await
            .ok()
            .flatten()
            .is_some();
        self.yielded();
        Ok(!is_child)
    }

    async fn profiles(&mut self) -> Result<Vec<delegation::AgentProfileInfo>> {
        self.budget.entered_host("profiles");
        Ok(self
            .grip()
            .cfg
            .subagents
            .profiles
            .iter()
            .map(|p| delegation::AgentProfileInfo {
                id: p.id.clone(),
                label: p.label.clone(),
                description: p.description.clone(),
                model: p.model.clone(),
                mode: p.mode.clone(),
            })
            .collect())
    }

    async fn limits(&mut self) -> Result<delegation::DelegationLimits> {
        self.budget.entered_host("limits");
        let cfg = &self.grip().cfg;
        Ok(delegation::DelegationLimits {
            max_children: cfg.subagents.max_children.min(self.policy.max_children) as u32,
            max_wait_secs: cfg.subagents.max_wait_secs,
            max_result_bytes: cfg.subagents.max_result_bytes as u32,
        })
    }

    async fn spawn(
        &mut self,
        req: delegation::SpawnRequest,
    ) -> Result<std::result::Result<delegation::SubagentInfo, String>> {
        self.budget.entered_host("spawn");
        self.require(crate::policy::Cap::Delegation)?;
        let parent = self.delegating_session()?;
        let grip = self.grip.clone();
        let out = crate::delegation::spawn(
            &grip,
            &parent,
            crate::delegation::SpawnRequest {
                label: req.label,
                task: req.task,
                profile: req.profile,
                model: req.model,
                mode: req.mode,
            },
        )
        .await
        .map(|row| info_from_row(&row, grip.cfg.subagents.max_result_bytes))
        .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn children(&mut self) -> Result<Vec<delegation::SubagentInfo>> {
        self.budget.entered_host("children");
        let parent = self.delegating_session()?;
        let grip = self.grip.clone();
        let rows = grip.persist.subagents_of(&parent).await.unwrap_or_default();
        let cap = grip.cfg.subagents.max_result_bytes;
        let infos = infos_from_rows(&grip, &rows, cap).await;
        self.yielded();
        Ok(infos)
    }

    /// Blocks the parent's turn until a predicate holds or the deadline passes.
    ///
    /// Returns the whole child list rather than only what the predicate
    /// matched, because a parent that has just been woken almost always wants
    /// to know the state of everything it started, and a second call to get it
    /// would be a wasted round trip through the guest boundary.
    async fn wait(
        &mut self,
        until: String,
        children: Vec<String>,
        timeout_secs: u64,
    ) -> Result<std::result::Result<delegation::WaitResult, String>> {
        self.budget.entered_host("wait");
        let parent = self.delegating_session()?;
        let grip = self.grip.clone();
        let cap = grip.cfg.subagents.max_result_bytes;

        let predicate = match crate::delegation::WaitFor::parse(&until, children) {
            Ok(p) => p,
            Err(e) => return Ok(Err(format!("{e:#}"))),
        };
        let timeout = std::time::Duration::from_secs(timeout_secs.max(1));

        // `delegation::wait` races the stop signal itself so it can leave its
        // child snapshot internally consistent. Unlike ordinary tool components,
        // though, that means it returns through this dedicated host import rather
        // than `tooling::invoke`'s `interruptible` wrapper. Carry the stop into the
        // agent store's budget here as well: the wait result may be rendered as a
        // tool error, but the stopped turn must not resume Wasm and issue another
        // tool call if its inbox checkpoint is buggy or delayed.
        let out = match crate::delegation::wait(&grip, &parent, &predicate, timeout).await {
            Ok(o) => {
                let children = infos_from_rows(&grip, &o.children, cap).await;
                Ok(delegation::WaitResult {
                    reason: o.reason,
                    timed_out: o.timed_out,
                    children,
                })
            }
            Err(e) => Err(format!("{e:#}")),
        };
        if self.cancelled() {
            self.budget.cancel();
        }
        self.yielded();
        Ok(out)
    }

    async fn cancel_child(
        &mut self,
        child_id: String,
    ) -> Result<std::result::Result<delegation::SubagentInfo, String>> {
        self.budget.entered_host("cancel_child");
        let parent = self.delegating_session()?;
        let grip = self.grip.clone();
        let out = crate::delegation::cancel_child(&grip, &parent, &child_id)
            .await
            .map(|row| info_from_row(&row, grip.cfg.subagents.max_result_bytes))
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    /// A child's transcript, for the rare case where the parent needs more than
    /// the final answer — a failure it wants to diagnose, usually.
    ///
    /// Scoped to this session's own children on purpose: without the check this
    /// would be a way to read any conversation in the database by id.
    async fn child_transcript(
        &mut self,
        child_id: String,
        from_seq: u64,
    ) -> Result<std::result::Result<Vec<EventRecord>, String>> {
        self.budget.entered_host("child_transcript");
        let parent = self.delegating_session()?;
        let grip = self.grip.clone();
        let row = grip.persist.get_subagent(&child_id).await.ok().flatten();
        let out = match row {
            Some(row) if row.parent_id == parent => grip
                .persist
                .events(&child_id, from_seq)
                .await
                .map_err(|e| format!("{e:#}")),
            _ => Err(format!(
                "`{child_id}` is not one of this session's sub-agents"
            )),
        };
        self.yielded();
        Ok(out)
    }
}

/// A child list for the parent to read, with live numbers on anything still
/// running.
///
/// The registry row is authoritative once a child has settled, and silent
/// before then: `cost_usd` stays 0.0 and there is nothing at all to say how
/// far the child has got. That silence is what made seven working sub-agents
/// look identical to seven hung ones, so a running row is topped up here from
/// the ledger and the child's own session metadata — one batched call for the
/// whole list, whatever its length.
async fn infos_from_rows(
    grip: &Arc<Grip>,
    rows: &[crate::subagents::SubagentRow],
    max_result_bytes: usize,
) -> Vec<delegation::SubagentInfo> {
    let mut infos: Vec<delegation::SubagentInfo> = rows
        .iter()
        .map(|r| info_from_row(r, max_result_bytes))
        .collect();

    let live: Vec<String> = rows
        .iter()
        .filter(|r| !r.state.is_terminal())
        .map(|r| r.child_id.clone())
        .collect();
    if live.is_empty() {
        return infos;
    }
    // Best effort on purpose: a status render that fails because the progress
    // read failed would be worse than one without the extra numbers.
    let progress = match grip.persist.session_progress(&live).await {
        Ok(p) if p.len() == live.len() => p,
        Ok(_) => return infos,
        Err(e) => {
            tracing::debug!(error = %e, "sub-agent progress was not read");
            return infos;
        }
    };
    for (id, p) in live.iter().zip(progress) {
        if let Some(info) = infos.iter_mut().find(|i| &i.id == id) {
            info.cost_usd = p.cost_usd;
            info.events = p.events;
            info.activity_ms = p.activity_ms;
        }
    }
    infos
}

/// Wire form of a registry row, with the answer clamped to what the parent's
/// context should carry.
fn info_from_row(
    row: &crate::subagents::SubagentRow,
    max_result_bytes: usize,
) -> delegation::SubagentInfo {
    delegation::SubagentInfo {
        id: row.child_id.clone(),
        label: row.label.clone(),
        task: row.task.clone(),
        profile: row.agent_aspect.clone(),
        model: row.model.clone(),
        mode: row.mode.clone(),
        state: row.state.as_str().to_string(),
        answer: crate::delegation::clamp_result(&row.result, max_result_bytes),
        detail: row.detail.clone(),
        cost_usd: row.cost_usd,
        created_ms: row.created_ms,
        finished_ms: row.finished_ms,
        // Filled in by `infos_from_rows` for a child that is still running;
        // a settled child's log is history, and its cost is on the row.
        events: 0,
        activity_ms: 0,
    }
}

// --- transcripts ------------------------------------------------------------

/// Reading and searching conversation logs owned by the calling principal.
///
/// Recall spans multiple conversations, so these calls are not pinned by
/// `scope_ok`; instead the gateway-side persistence service resolves the
/// caller's root owner and filters every catalogue/read/search operation to
/// that owner. The interface remains read-only, and every scan is offloaded so
/// host time is not charged as guest spin.
impl transcripts::Host for HostState {
    async fn conversations(
        &mut self,
        include_archived: bool,
        include_subagents: bool,
        limit: u64,
    ) -> Result<std::result::Result<Vec<transcripts::ConversationSummary>, String>> {
        self.budget.entered_host("conversations");
        self.require(crate::policy::Cap::Transcripts)?;
        let grip = self.grip.clone();
        let out = grip
            .persist
            .conversations(include_archived, include_subagents, limit as usize)
            .await
            .map(|rows| rows.iter().map(summary_to_wit).collect())
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn conversation(
        &mut self,
        session_id: String,
    ) -> Result<std::result::Result<transcripts::ConversationSummary, String>> {
        self.budget.entered_host("conversation");
        self.require(crate::policy::Cap::Transcripts)?;
        let grip = self.grip.clone();
        // Served through the catalogue rather than a dedicated call: one
        // conversation is the same query with a filter, and adding an IPC arm
        // for it would be a second thing to keep in step with the first.
        let out = grip
            .persist
            .conversations(true, true, 0)
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|rows| {
                rows.iter()
                    .find(|c| c.id == session_id)
                    .map(summary_to_wit)
                    .ok_or_else(|| format!("no conversation with id `{session_id}`"))
            });
        self.yielded();
        Ok(out)
    }

    async fn subagents(
        &mut self,
        root_id: String,
    ) -> Result<std::result::Result<Vec<transcripts::ConversationSummary>, String>> {
        self.budget.entered_host("subagents");
        self.require(crate::policy::Cap::Transcripts)?;
        let grip = self.grip.clone();
        let out = grip
            .persist
            .conversation_subagents(&root_id)
            .await
            .map(|rows| rows.iter().map(summary_to_wit).collect())
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn read(
        &mut self,
        session_id: String,
        from_seq: u64,
        limit: u64,
        max_chars: u64,
    ) -> Result<std::result::Result<Vec<transcripts::TranscriptEntry>, String>> {
        self.budget.entered_host("read");
        self.require(crate::policy::Cap::Transcripts)?;
        let grip = self.grip.clone();
        let out = grip
            .persist
            .read_transcript(&session_id, from_seq, limit as usize, max_chars as usize)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|e| transcripts::TranscriptEntry {
                        seq: e.seq,
                        ts_ms: e.ts_ms,
                        kind: e.kind,
                        text: e.text,
                        elided: e.elided,
                    })
                    .collect()
            })
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }

    async fn search(
        &mut self,
        query: transcripts::SearchQuery,
    ) -> Result<std::result::Result<transcripts::SearchReport, String>> {
        self.budget.entered_host("search");
        self.require(crate::policy::Cap::Transcripts)?;
        let grip = self.grip.clone();
        let out = grip
            .persist
            .search_transcripts(&crate::transcripts::SearchQuery {
                pattern: query.pattern,
                session_id: query.session_id,
                include_archived: query.include_archived,
                include_subagents: query.include_subagents,
                include_tool_output: query.include_tool_output,
                max_results: query.max_results as usize,
                max_chars: query.max_chars as usize,
            })
            .await
            .map(|r| transcripts::SearchReport {
                hits: r
                    .hits
                    .into_iter()
                    .map(|h| transcripts::TranscriptHit {
                        session_id: h.session_id,
                        title: h.title,
                        is_subagent: h.is_subagent,
                        label: h.label,
                        seq: h.seq,
                        ts_ms: h.ts_ms,
                        kind: h.kind,
                        text: h.text,
                    })
                    .collect(),
                matched_conversations: r.matched_conversations,
                total_matches: r.total_matches,
                scanned_conversations: r.scanned_conversations,
                capped: r.capped,
                incomplete: r.incomplete,
            })
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(out)
    }
}

fn summary_to_wit(c: &crate::transcripts::ConversationSummary) -> transcripts::ConversationSummary {
    transcripts::ConversationSummary {
        id: c.id.clone(),
        title: c.title.clone(),
        mode: c.mode.clone(),
        model: c.model.clone(),
        preview: c.preview.clone(),
        created_ms: c.created_ms,
        updated_ms: c.updated_ms,
        event_count: c.event_count,
        archived: c.archived,
        is_subagent: c.is_subagent,
        parent_id: c.parent_id.clone(),
        root_id: c.root_id.clone(),
        label: c.label.clone(),
        state: c.state.clone(),
        task: c.task.clone(),
    }
}

// --- host filesystem --------------------------------------------------------

// Every direct call below is synchronous filesystem work on whatever runtime
// thread the guest's import landed on — a whole-file read, a recursive delete,
// a directory walk over a large tree. `search_files`/`find_files` already went
// to the blocking pool; these are offloaded for the same reason.
impl hostfs::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        self.budget.entered_host("available");
        Ok(self.grip().cfg.filesystem.enabled
            && !self.policy.denies(crate::policy::Cap::FilesystemRead))
    }

    async fn read_file(&mut self, path: String) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("read_file");
        self.require(crate::policy::Cap::FilesystemRead)?;
        let grip = self.grip.clone();
        Ok(
            crate::offload::blocking(|| crate::hostfs::read_file(&grip.cfg, &path))
                .map(|text| grip.truncate(text))
                .map_err(|e| format!("{e:#}")),
        )
    }

    async fn write_file(
        &mut self,
        path: String,
        contents: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("write_file");
        self.require(crate::policy::Cap::FilesystemWrite)?;
        let grip = self.grip.clone();
        Ok(
            crate::offload::blocking(|| crate::hostfs::write_file(&grip.cfg, &path, &contents))
                .map_err(|e| format!("{e:#}")),
        )
    }

    async fn list_dir(
        &mut self,
        path: String,
    ) -> Result<std::result::Result<Vec<FsEntry>, String>> {
        self.budget.entered_host("list_dir");
        self.require(crate::policy::Cap::FilesystemRead)?;
        let grip = self.grip.clone();
        Ok(
            crate::offload::blocking(|| crate::hostfs::list_dir(&grip.cfg, &path))
                .map_err(|e| format!("{e:#}")),
        )
    }

    async fn delete_path(
        &mut self,
        path: String,
        recursive: bool,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("delete_path");
        self.require(crate::policy::Cap::FilesystemDelete)?;
        let grip = self.grip.clone();
        let result =
            crate::offload::blocking(|| crate::hostfs::delete_path(&grip.cfg, &path, recursive));
        if let Ok(message) = &result {
            // Deletions are worth a line in the log whoever asked for them.
            tracing::warn!(%path, "agent deleted a path: {message}");
        }
        Ok(result.map_err(|e| format!("{e:#}")))
    }

    async fn read_file_range(
        &mut self,
        path: String,
        offset: u32,
        limit: u32,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("read_file_range");
        self.require(crate::policy::Cap::FilesystemRead)?;
        let grip = self.grip.clone();
        Ok(crate::offload::blocking(|| {
            crate::hostfs::read_file_range(&grip.cfg, &path, offset, limit)
        })
        .map(|text| grip.truncate(text))
        .map_err(|e| format!("{e:#}")))
    }

    async fn edit_file(
        &mut self,
        path: String,
        old_text: String,
        new_text: String,
        replace_all: bool,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("edit_file");
        self.require(crate::policy::Cap::FilesystemWrite)?;
        let grip = self.grip.clone();
        Ok(crate::offload::blocking(|| {
            crate::hostfs::edit_file(&grip.cfg, &path, &old_text, &new_text, replace_all)
        })
        .map_err(|e| format!("{e:#}")))
    }

    async fn search_files(
        &mut self,
        pattern: String,
        path: Option<String>,
        glob: Option<String>,
        mode: String,
        max_results: u32,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("search_files");
        self.require(crate::policy::Cap::FilesystemRead)?;
        let grip = self.grip.clone();
        // Searching a large tree blocks on the filesystem for long enough to
        // starve the runtime's other tasks, so it runs on the blocking pool.
        let cfg = grip.cfg.clone();
        let joined = tokio::task::spawn_blocking(move || {
            crate::hostfs::search_files(
                &cfg,
                &pattern,
                path.as_deref(),
                glob.as_deref(),
                &mode,
                max_results,
            )
        })
        .await;
        // A search that died takes the answer with it, not the guest: the agent
        // can retry or narrow, which it cannot do if the call traps.
        Ok(match joined {
            Ok(result) => result
                .map(|text| grip.truncate(text))
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("the search did not finish: {e}")),
        })
    }

    async fn find_files(
        &mut self,
        glob: String,
        path: Option<String>,
        max_results: u32,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("find_files");
        self.require(crate::policy::Cap::FilesystemRead)?;
        let grip = self.grip.clone();
        let cfg = grip.cfg.clone();
        let joined = tokio::task::spawn_blocking(move || {
            crate::hostfs::find_files(&cfg, &glob, path.as_deref(), max_results)
        })
        .await;
        Ok(match joined {
            Ok(result) => result
                .map(|text| grip.truncate(text))
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("the search did not finish: {e}")),
        })
    }
}

// --- terminals --------------------------------------------------------------

impl terminal::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        self.budget.entered_host("available");
        Ok(self.grip().cfg.terminal.enabled && !self.policy.denies(crate::policy::Cap::Terminal))
    }

    async fn open(&mut self, spec: TerminalOpen) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("open");
        self.require(crate::policy::Cap::Terminal)?;
        if spec.host.is_some() {
            self.require(crate::policy::Cap::Ssh)?;
        }
        let grip = self.grip.clone();
        let result = grip
            .terminals
            .open(
                &grip.cfg,
                crate::terminal::OpenSpec {
                    cwd: spec.cwd,
                    name: spec.name,
                    env: spec.env.into_iter().map(|v| (v.key, v.value)).collect(),
                    host: spec.host,
                },
            )
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn signal(
        &mut self,
        id: String,
        signal: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("signal");
        self.require(crate::policy::Cap::Terminal)?;
        let grip = self.grip.clone();
        tracing::info!(terminal = %id, %signal, "signalling a session");
        let result = grip
            .terminals
            .signal(&grip.cfg, &id, &signal)
            .await
            .map(|text| grip.truncate(text))
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn send(
        &mut self,
        id: String,
        text: String,
        submit: bool,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("send");
        self.require(crate::policy::Cap::Terminal)?;
        let grip = self.grip.clone();
        let result = grip
            .terminals
            .send(&grip.cfg, &id, &text, submit, grip.cfg.terminal.send_settle)
            .await
            .map(|out| grip.truncate(out))
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn run(
        &mut self,
        id: String,
        command: String,
        timeout_ms: u32,
        background: bool,
    ) -> Result<std::result::Result<TerminalOutput, String>> {
        self.budget.entered_host("run");
        self.require(crate::policy::Cap::Terminal)?;
        let grip = self.grip.clone();
        let timeout = if timeout_ms == 0 {
            grip.cfg.terminal.default_timeout
        } else {
            std::time::Duration::from_millis(timeout_ms as u64)
        };

        tracing::info!(terminal = %id, %command, "running a command");
        // A stop must not have to wait out the command's timeout, which is
        // exactly the wait it is trying to cut short. The shell keeps running —
        // killing it would leave the session in an unknown state — but the
        // output collected so far comes back straight away.
        let cancel = self.cancel_flag();
        let result = self
            .interruptible(
                "the command",
                grip.terminals
                    .run_until(&grip.cfg, &id, &command, timeout, background, cancel),
            )
            .await;
        // A command can legitimately take a long time; that is not the guest
        // spinning, so the budget's spin timer restarts here.
        self.yielded();
        Ok(match result {
            Ok(inner) => inner.map_err(|e| format!("{e:#}")),
            Err(stopped) => Err(stopped),
        })
    }

    async fn read(&mut self, id: String) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("read");
        self.require(crate::policy::Cap::Terminal)?;
        let grip = self.grip.clone();
        Ok(grip
            .terminals
            .read(&id)
            .await
            .map(|text| grip.truncate(text))
            .map_err(|e| format!("{e:#}")))
    }

    async fn close(&mut self, id: String) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("close");
        self.require(crate::policy::Cap::Terminal)?;
        let grip = self.grip.clone();
        let result = grip
            .terminals
            .close(&id)
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn sessions(&mut self) -> Result<Vec<TerminalInfo>> {
        self.budget.entered_host("sessions");
        self.require(crate::policy::Cap::Terminal)?;
        let grip = self.grip.clone();
        let list = grip.terminals.list().await;
        self.yielded();
        Ok(list)
    }

    // --- the named-host registry -------------------------------------------

    async fn ssh_available(&mut self) -> Result<bool> {
        self.budget.entered_host("ssh-available");
        let cfg = &self.grip().cfg;
        Ok(cfg.terminal.enabled
            && cfg.terminal.ssh_enabled
            && !self.policy.denies(crate::policy::Cap::Ssh))
    }

    async fn ssh_hosts(&mut self) -> Result<std::result::Result<Vec<SshHostInfo>, String>> {
        self.budget.entered_host("ssh-hosts");
        self.require(crate::policy::Cap::Ssh)?;
        let grip = self.grip.clone();
        Ok(crate::sshhosts::list(&grip.cfg)
            .map(|hosts| hosts.iter().map(host_info).collect())
            .map_err(|e| format!("{e:#}")))
    }

    async fn ssh_host_set(
        &mut self,
        host: SshHostInfo,
        merge: bool,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("ssh-host-set");
        self.require(crate::policy::Cap::Ssh)?;
        let grip = self.grip.clone();
        let result = crate::sshhosts::set(&grip.cfg, from_host_info(host), merge)
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn ssh_host_remove(
        &mut self,
        name: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("ssh-host-remove");
        self.require(crate::policy::Cap::Ssh)?;
        let grip = self.grip.clone();
        let result = crate::sshhosts::remove(&grip.cfg, &name).map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn ssh_host_rename(
        &mut self,
        from: String,
        to: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("ssh-host-rename");
        self.require(crate::policy::Cap::Ssh)?;
        let grip = self.grip.clone();
        let result = crate::sshhosts::rename(&grip.cfg, &from, &to).map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }
}

fn host_info(host: &crate::sshhosts::SshHost) -> SshHostInfo {
    SshHostInfo {
        name: host.name.clone(),
        host: host.host.clone(),
        port: host.port,
        user: host.user.clone(),
        identity_file: host.identity_file.clone(),
        options: host.options.clone(),
        remote_cwd: host.remote_cwd.clone(),
        pty: host.pty,
        description: host.description.clone(),
    }
}

fn from_host_info(info: SshHostInfo) -> crate::sshhosts::SshHost {
    crate::sshhosts::SshHost {
        name: info.name,
        host: info.host,
        port: info.port,
        user: info.user,
        identity_file: info.identity_file,
        options: info.options,
        remote_cwd: info.remote_cwd,
        pty: info.pty,
        description: info.description,
    }
}

// --- process control --------------------------------------------------------

impl control::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        self.budget.entered_host("available");
        Ok(self.grip().cfg.control.allow_restart
            && !self.policy.denies(crate::policy::Cap::Control))
    }

    async fn restart(
        &mut self,
        reason: String,
        resume: bool,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("restart");
        self.require(crate::policy::Cap::Control)?;
        let grip = self.grip.clone();
        let session = self.session_id.clone();
        Ok(
            crate::control::request_restart(&grip, &reason, resume, session.as_deref())
                .await
                .map_err(|e| format!("{e:#}")),
        )
    }
}

// --- configuration ----------------------------------------------------------

fn entry(setting: crate::settings::Setting) -> ConfigEntry {
    ConfigEntry {
        key: setting.key,
        value: setting.value,
        editable: setting.editable,
        live: setting.live,
    }
}

impl configuration::Host for HostState {
    async fn settings(&mut self, prefix: Option<String>) -> Result<Vec<ConfigEntry>> {
        self.budget.entered_host("settings");
        let grip = self.grip();
        Ok(crate::settings::list(&grip.cfg, prefix.as_deref())
            .unwrap_or_default()
            .into_iter()
            .map(entry)
            .collect())
    }

    async fn get(&mut self, key: String) -> Result<Option<ConfigEntry>> {
        self.budget.entered_host("get");
        let grip = self.grip();
        Ok(crate::settings::get(&grip.cfg, &key)
            .ok()
            .flatten()
            .map(entry))
    }

    async fn set(
        &mut self,
        key: String,
        value: String,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("set");
        self.require(crate::policy::Cap::ConfigWrite)?;
        let grip = self.grip.clone();
        let result = crate::settings::set(&grip.cfg, &key, &value);
        self.yielded();
        Ok(result.map_err(|e| format!("{e:#}")))
    }
}

// --- skills ---------------------------------------------------------------
//
// The host side of the `skills` and `skills-view` interfaces.
//
// Two interfaces over one corpus, deliberately: the agent gets `skills`, which
// can write, while the chat surface gets `skills-view`, which cannot. A gateway
// renders what the agent knows; it does not author it. Splitting them here is
// what makes that enforceable rather than merely intended, because the linker
// only hands each guest the interface its capability allows.
//
// All the real work lives in `skill_manager`. These functions map its plain
// structs onto the generated WIT records and mark the watchdog around anything
// that waits on the network.

use crate::bindings::types::{SkillBody, SkillCard, SkillDiagnostic, SkillWrite};
use crate::bindings::{skills, skills_view};
use crate::skill_manager as sm;

fn card(c: sm::Card) -> SkillCard {
    SkillCard {
        id: c.id,
        parent: c.parent,
        name: c.name,
        brief: c.brief,
        when_to_use: c.when_to_use,
        tags: c.tags,
        children: c.children,
        universal: c.universal,
        resources: c.resources,
        related: c.related,
        status: c.status,
        superseded_by: c.superseded_by,
        score: c.score,
        how: c.how,
    }
}

fn cards(v: Vec<sm::Card>) -> Vec<SkillCard> {
    v.into_iter().map(card).collect()
}

fn diag(d: sm::Diag) -> SkillDiagnostic {
    SkillDiagnostic {
        id: d.id,
        severity: d.severity,
        message: d.message,
    }
}

fn diags(v: Vec<sm::Diag>) -> Vec<SkillDiagnostic> {
    v.into_iter().map(diag).collect()
}

/// Character counts are `usize` on the host and `u32` in the contract. Clamping
/// rather than casting, so a body longer than 4GB would report a saturated
/// length instead of a wrapped one. It cannot happen, but a silent wrap is the
/// kind of thing that stops being impossible later.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn body(b: sm::Body) -> SkillBody {
    SkillBody {
        id: b.id,
        name: b.name,
        file: b.resource,
        content: b.content,
        resources: b.resources,
        children: b.children,
        offset: clamp(b.offset),
        total: clamp(b.total),
        truncated: b.truncated,
    }
}

fn write_outcome(w: sm::WriteOutcome) -> SkillWrite {
    SkillWrite {
        id: w.id,
        path: w.path,
        created: w.created,
        diagnostics: diags(w.diagnostics),
    }
}

impl skills::Host for HostState {
    async fn universal(&mut self) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("universal");
        let mgr = self.grip().skills.clone();
        let out = cards(mgr.universal());
        self.yielded();
        Ok(out)
    }

    async fn retrieve(
        &mut self,
        session_id: String,
        query: String,
        limit: u32,
    ) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("retrieve");
        self.scope_ok(&session_id)?;
        let mgr = self.grip().skills.clone();
        // Embedding is a network round trip. Mark the yield on both sides so a
        // slow provider reads as waiting on the host, not as a spinning guest.
        self.yielded();
        let out = mgr.retrieve(&session_id, &query, limit as usize).await;
        self.yielded();
        Ok(cards(out))
    }

    async fn search(&mut self, query: String, limit: u32) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("search");
        let mgr = self.grip().skills.clone();
        self.yielded();
        let out = mgr.search(&query, limit as usize).await;
        self.yielded();
        Ok(cards(out))
    }

    async fn pinned(&mut self, session_id: String) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("pinned");
        self.scope_ok(&session_id)?;
        let mgr = self.grip().skills.clone();
        let out = cards(mgr.pinned(&session_id).await);
        self.yielded();
        Ok(out)
    }

    async fn pin(
        &mut self,
        session_id: String,
        ids: Vec<String>,
    ) -> Result<std::result::Result<Vec<SkillCard>, String>> {
        self.budget.entered_host("pin");
        self.scope_ok(&session_id)?;
        let mgr = self.grip().skills.clone();
        let out = mgr
            .pin(&session_id, &ids)
            .await
            .map(cards)
            .map_err(|e| e.to_string());
        self.yielded();
        Ok(out)
    }

    async fn fetch(
        &mut self,
        id: String,
        file: String,
        offset: u32,
        limit: u32,
    ) -> Result<std::result::Result<SkillBody, String>> {
        self.budget.entered_host("fetch");
        let mgr = self.grip().skills.clone();
        let out = mgr
            .fetch(&id, &file, offset as usize, limit as usize)
            .map(body)
            .map_err(|e| e.to_string());
        self.yielded();
        Ok(out)
    }

    async fn upsert(
        &mut self,
        id: String,
        file: String,
        contents: String,
    ) -> Result<std::result::Result<SkillWrite, String>> {
        self.budget.entered_host("upsert");
        self.require(crate::policy::Cap::SkillsWrite)?;
        let mgr = self.grip().skills.clone();
        let out = mgr
            .upsert(&id, &file, &contents)
            .map(write_outcome)
            .map_err(|e| e.to_string());
        if out.is_ok() {
            // Skills are plain files with no build step; the commit is their
            // entire revision history.
            let _ = self
                .grip
                .commit_worktree(&format!("skill: upsert {id}"))
                .await;
        }
        self.yielded();
        Ok(out)
    }

    async fn remove(
        &mut self,
        id: String,
        recursive: bool,
    ) -> Result<std::result::Result<String, String>> {
        self.budget.entered_host("remove");
        self.require(crate::policy::Cap::SkillsWrite)?;
        let mgr = self.grip().skills.clone();
        let out = mgr.remove(&id, recursive).map_err(|e| e.to_string());
        if out.is_ok() {
            let _ = self
                .grip
                .commit_worktree(&format!("skill: remove {id}"))
                .await;
        }
        self.yielded();
        Ok(out)
    }

    async fn lint(&mut self, id: String) -> Result<Vec<SkillDiagnostic>> {
        self.budget.entered_host("lint");
        let mgr = self.grip().skills.clone();
        let out = diags(mgr.lint(&id));
        self.yielded();
        Ok(out)
    }
}

impl skills_view::Host for HostState {
    /// Every skill, parents before children, for a tree view.
    async fn all(&mut self) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("all");
        let mgr = self.grip().skills.clone();
        let out = cards(mgr.all());
        self.yielded();
        Ok(out)
    }

    async fn universal(&mut self) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("universal");
        let mgr = self.grip().skills.clone();
        let out = cards(mgr.universal());
        self.yielded();
        Ok(out)
    }

    /// Unscoped on purpose: managing every session is the gateway's job, which
    /// is why `scope_ok` is not called here. It stays read-only regardless.
    async fn pinned(&mut self, session_id: String) -> Result<Vec<SkillCard>> {
        self.budget.entered_host("pinned");
        let mgr = self.grip().skills.clone();
        let out = cards(mgr.pinned(&session_id).await);
        self.yielded();
        Ok(out)
    }

    async fn lint(&mut self) -> Result<Vec<SkillDiagnostic>> {
        self.budget.entered_host("lint");
        let mgr = self.grip().skills.clone();
        let out = diags(mgr.lint(""));
        self.yielded();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn delegation_wait_propagates_a_web_stop_into_the_agent_budget() {
        // `wait` is the one tool path that intentionally bypasses
        // `HostState::interruptible`, so keep its equivalent cancellation
        // handoff explicit. Otherwise a stopped wait can return to Wasm and let
        // the turn continue running tools until its next inbox checkpoint.
        let src = include_str!("host_api.rs");
        let body = src
            .split("async fn wait(")
            .nth(1)
            .expect("delegation wait moved")
            .split("    async fn cancel_child(")
            .next()
            .unwrap_or_default();
        assert!(
            body.contains("if self.cancelled()") && body.contains("self.budget.cancel()"),
            "delegation wait no longer carries a web stop into the agent budget"
        );
    }

    /// Every function on the `session` interface that names a session and can
    /// change it must call `scope_ok`, so an agent turn cannot reach into a
    /// conversation that is not its own.
    ///
    /// Checked by reading this file rather than by calling the functions:
    /// `scope_ok` needs a whole `HostState`, which owns a WASI context, a grip,
    /// a store and a budget, and standing one up would test the fixture more
    /// than the rule. The rule is textual anyway — the bug it guards against is
    /// a new method written without the line, and that is exactly what a source
    /// check catches.
    ///
    /// Why it matters beyond privacy: an unscoped `submit` is half of a
    /// delegation bypass. A session an agent creates itself is in no sub-agent
    /// registry, so it has no parent, escapes the fan-out cap, and — since the
    /// one-level rule is decided by registry membership — could delegate
    /// further. `create_session` refuses an agent outright for the same reason,
    /// and is checked here too.
    #[test]
    fn every_session_mutator_is_scoped_to_its_own_session() {
        let src = include_str!("host_api.rs");
        let session_impl = src
            .split("impl session::Host for HostState {")
            .nth(1)
            .expect("the session host impl moved or was renamed");

        // Functions that take a session id and mutate something. Read-only
        // lookups are deliberately absent: `get_session` and `list_sessions`
        // are how a conversation is named in a picker, and `available_tools`
        // answers for whichever session the UI is showing.
        for method in [
            "async fn append(",
            "async fn emit_output(",
            "async fn emit_reasoning(",
            "async fn emit_compaction_progress(",
            "async fn poll_inbox(",
            "async fn events(",
            "async fn submit(",
            "async fn rename_session(",
            "async fn archive_session(",
            "async fn set_session_mode(",
            "async fn set_session_model(",
        ] {
            let body = session_impl
                .split(method)
                .nth(1)
                .unwrap_or_else(|| panic!("`{method}` is gone from the session impl"));
            // Up to the next method, so a later `scope_ok` cannot satisfy this.
            let body = body.split("    async fn ").next().unwrap_or(body);
            assert!(
                body.contains("self.scope_ok(&session_id)?")
                    || body.contains("self.may_access(&session_id)?"),
                "`{method}` does not call scope_ok, so an agent turn can use it \
                 on another conversation's session. Add \
                 `self.scope_ok(&session_id)?;` after the budget line, or — if \
                 it genuinely must be unscoped — say why in a comment and \
                 remove it from this list."
            );
        }
    }

    /// Every host import that can change something or reach something calls
    /// `require` with the capability the policy table names for it. This is
    /// the hard boundary of multi-user mode: the agent is rewritable, so a
    /// tool it withholds is advisory, and only a refusal here is authoritative.
    ///
    /// Checked textually for the same reason `every_session_mutator_is_scoped`
    /// is: the bug this guards against is a new import written without the
    /// line, and a source check catches exactly that. The pairs mirror the
    /// enforcement matrix in `docs/plans/multi-user.md` §3.7.
    #[test]
    fn every_guarded_import_requires_its_capability() {
        let src = include_str!("host_api.rs");
        let matrix: &[(&str, &[(&str, &str)])] = &[
            ("impl hostfs::Host for HostState {", &[
                ("async fn read_file(", "FilesystemRead"),
                ("async fn read_file_range(", "FilesystemRead"),
                ("async fn list_dir(", "FilesystemRead"),
                ("async fn search_files(", "FilesystemRead"),
                ("async fn find_files(", "FilesystemRead"),
                ("async fn write_file(", "FilesystemWrite"),
                ("async fn edit_file(", "FilesystemWrite"),
                ("async fn delete_path(", "FilesystemDelete"),
            ]),
            ("impl terminal::Host for HostState {", &[
                ("async fn open(", "Terminal"),
                ("async fn run(", "Terminal"),
                ("async fn read(", "Terminal"),
                ("async fn send(", "Terminal"),
                ("async fn signal(", "Terminal"),
                ("async fn close(", "Terminal"),
                ("async fn sessions(", "Terminal"),
                ("async fn ssh_hosts(", "Ssh"),
                ("async fn ssh_host_set(", "Ssh"),
                ("async fn ssh_host_remove(", "Ssh"),
                ("async fn ssh_host_rename(", "Ssh"),
            ]),
            ("impl control::Host for HostState {", &[("async fn restart(", "Control")]),
            ("impl configuration::Host for HostState {", &[("async fn set(", "ConfigWrite")]),
            ("impl devkit::Host for HostState {", &[
                ("async fn new_tool(", "Devkit"),
                ("async fn write_file(", "Devkit"),
                ("async fn patch_file(", "Devkit"),
                ("async fn add_dependency(", "Devkit"),
                ("async fn remove_dependency(", "Devkit"),
            ]),
            ("impl branch::Host for HostState {", &[
                ("async fn update_from_trunk(", "BranchWrite"),
                ("async fn reset_to(", "BranchWrite"),
                ("async fn complete_merge(", "BranchWrite"),
                ("async fn abort_merge(", "BranchWrite"),
            ]),
            ("impl delegation::Host for HostState {", &[("async fn spawn(", "Delegation")]),
            ("impl skills::Host for HostState {", &[
                ("async fn upsert(", "SkillsWrite"),
                ("async fn remove(", "SkillsWrite"),
            ]),
            ("impl transcripts::Host for HostState {", &[
                ("async fn conversations(", "Transcripts"),
                ("async fn conversation(", "Transcripts"),
                ("async fn subagents(", "Transcripts"),
                ("async fn read(", "Transcripts"),
                ("async fn search(", "Transcripts"),
            ]),
            ("impl tooling::Host for HostState {", &[("async fn invoke(", "ComponentTools")]),
        ];
        for (marker, methods) in matrix {
            let block = src
                .split(marker)
                .nth(1)
                .unwrap_or_else(|| panic!("`{marker}` moved or was renamed"));
            // Up to the next impl block, so a method of the same name on
            // another interface cannot satisfy this.
            let block = block.split("\nimpl ").next().unwrap_or(block);
            for (method, cap) in *methods {
                let body = block
                    .split(method)
                    .nth(1)
                    .unwrap_or_else(|| panic!("`{method}` is gone from `{marker}`"));
                let body = body.split("    async fn ").next().unwrap_or(body);
                let want = format!("self.require(crate::policy::Cap::{cap})?");
                assert!(
                    body.contains(&want),
                    "`{method}` in `{marker}` does not call `{want}`, so a role that \
                     withholds `{cap}` is not enforced there. Add it after the \
                     budget line, or remove the row from the matrix and say why."
                );
            }
        }

        // The `available()` probes are what make a withheld family vanish
        // from the agent's prompt; each must consult the policy.
        for (marker, cap) in [
            ("impl hostfs::Host for HostState {", "FilesystemRead"),
            ("impl terminal::Host for HostState {", "Terminal"),
            ("impl control::Host for HostState {", "Control"),
            ("impl delegation::Host for HostState {", "Delegation"),
        ] {
            let block = src.split(marker).nth(1).unwrap();
            let block = block.split("\nimpl ").next().unwrap_or(block);
            let body = block.split("async fn available(").nth(1).unwrap();
            let body = body.split("    async fn ").next().unwrap_or(body);
            assert!(
                body.contains(&format!("Cap::{cap}")),
                "`available()` under `{marker}` does not consult `Cap::{cap}`"
            );
        }
        let terminal = src.split("impl terminal::Host for HostState {").nth(1).unwrap();
        let ssh = terminal.split("async fn ssh_available(").nth(1).unwrap();
        assert!(ssh.split("    async fn ").next().unwrap().contains("Cap::Ssh"));

        // Catalogues and the model gate.
        let sys = src.split("impl sys::Host for HostState {").nth(1).unwrap();
        let sys = sys.split("\nimpl ").next().unwrap();
        assert!(sys.split("async fn list_models(").nth(1).unwrap().split("    async fn ").next().unwrap().contains("allows_model"));
        assert!(sys.split("async fn list_modes(").nth(1).unwrap().split("    async fn ").next().unwrap().contains("allows_mode"));
        let llm = src.split("impl llm::Host for HostState {").nth(1).unwrap();
        let llm = llm.split("\nimpl ").next().unwrap();
        for method in ["async fn chat(", "async fn stream_open("] {
            let body = llm.split(method).nth(1).unwrap().split("    async fn ").next().unwrap();
            assert!(body.contains("check_model("), "`{method}` does not gate the model");
            assert!(body.contains("check_budget("), "`{method}` does not gate spend");
        }
    }

    /// An agent must not be able to mint a conversation. `spawn_agent` is the
    /// sanctioned path, and it registers what it starts.
    #[test]
    fn an_agent_cannot_create_a_conversation() {
        let src = include_str!("host_api.rs");
        let body = src
            .split("async fn create_session(")
            .nth(1)
            .expect("create_session moved")
            .split("    async fn ")
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            body.contains("self.session_id.is_some()"),
            "create_session no longer refuses an agent turn: a session made \
             this way is in no sub-agent registry, so it has no parent, dodges \
             the fan-out cap, and could itself delegate"
        );
    }
}
