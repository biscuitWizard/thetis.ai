//! Where persistent state lives, from either side of the process split.
//!
//! redb permits one writer process, so the database belongs to the gateway
//! alone. Everything else — the worker running conversations — reaches the
//! same tables through the gateway over IPC. This enum is the seam: the same
//! call sites serve both roles, and a worker physically cannot contend for
//! the database because it never opens it.

use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::bindings::types::{EventRecord, SessionEvent, SessionMeta};
use crate::ipc::Peer;
use crate::store::{SessionProgress, Store};
use crate::subagents::SubagentRow;
use crate::transcripts::{ConversationSummary, SearchQuery, SearchReport, TranscriptEntry};

#[derive(Clone)]
pub enum Persist {
    /// The gateway: the one process holding the database open.
    Local(Arc<Store>),
    /// A worker: every access is a request to the gateway.
    Remote(Arc<Peer>),
}

macro_rules! delegate {
    // The pattern every method follows: run against the local store, or send
    // the same arguments over the wire and decode the same return type.
    ($self:ident, $method:literal, |$store:ident| $local:expr, $params:expr) => {
        match $self {
            Persist::Local($store) => crate::offload::blocking(|| $local),
            Persist::Remote(peer) => peer.call_as($method, $params).await,
        }
    };
}

impl Persist {
    // --- events -------------------------------------------------------------

    pub async fn append_event(&self, session_id: &str, event: SessionEvent) -> Result<EventRecord> {
        delegate!(
            self,
            "store.append_event",
            |s| s.append_event(session_id, event.clone()),
            json!({ "session": session_id, "event": event })
        )
    }

    pub async fn events(&self, session_id: &str, from_seq: u64) -> Result<Vec<EventRecord>> {
        delegate!(
            self,
            "store.events",
            |s| s.events(session_id, from_seq),
            json!({ "session": session_id, "from_seq": from_seq })
        )
    }

    // --- sessions -------------------------------------------------------------

    pub async fn create_session(
        &self,
        title: Option<String>,
        mode: &str,
        owner: &str,
    ) -> Result<SessionMeta> {
        delegate!(
            self,
            "store.create_session",
            |s| s.create_session(title.clone(), mode, owner),
            json!({ "title": title, "mode": mode, "owner": owner })
        )
    }

    pub async fn list_sessions_owned(
        &self,
        owner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionMeta>> {
        match self {
            Persist::Local(store) => {
                crate::offload::blocking(|| store.list_sessions_owned(owner, include_archived))
            }
            Persist::Remote(peer) => {
                peer.call_as(
                    "store.list_sessions",
                    json!({"include_archived": include_archived}),
                )
                .await
            }
        }
    }

    pub async fn owner_of_root(&self, id: &str) -> Result<Option<String>> {
        delegate!(
            self,
            "store.owner_of_root",
            |s| s.owner_of_root(id),
            json!({"id":id})
        )
    }

    /// Resolves the policy for a turn on the trusted gateway. Workers must
    /// not derive this from the configuration in their rewritable checkout.
    ///
    /// `speaker` is the account whose message started the turn. It is the whole
    /// point of this call: authority belongs to whoever is speaking, narrowed
    /// by the conversation's ceiling, so that being a participant in someone
    /// else's conversation never lends anybody their permissions. `None` means
    /// the turn has no identified speaker — a resume after a restart, or a
    /// legacy event — and falls back to the owner, which is what this did
    /// before ceilings existed.
    pub async fn session_policy(
        &self,
        id: &str,
        speaker: Option<&str>,
    ) -> Result<crate::policy::EffectivePolicy> {
        match self {
            Persist::Remote(peer) => {
                peer.call_as("store.session_policy", json!({"id": id, "speaker": speaker}))
                    .await
            }
            Persist::Local(_) => anyhow::bail!("session policy is resolved from gateway config"),
        }
    }

    /// The conversation's ceiling, if one was stamped.
    pub async fn ceiling_of(&self, id: &str) -> Result<Option<crate::policy::EffectivePolicy>> {
        delegate!(
            self,
            "store.ceiling_of",
            |s| s.ceiling_of(id),
            json!({"id": id})
        )
    }

    /// Stamps a conversation's ceiling. Gateway-side only in practice: the
    /// callers are session creation and `/fork`, never a guest.
    pub async fn set_ceiling(&self, id: &str, policy: &crate::policy::EffectivePolicy) -> Result<()> {
        delegate!(
            self,
            "store.set_ceiling",
            |s| s.set_ceiling(id, policy),
            json!({"id": id, "policy": policy})
        )
    }

    pub async fn is_participant(&self, id: &str, account: &str) -> Result<bool> {
        delegate!(
            self,
            "store.is_participant",
            |s| s.is_participant(id, account),
            json!({"id": id, "account": account})
        )
    }

    pub async fn participants(&self, id: &str) -> Result<Vec<crate::store::ParticipantRow>> {
        delegate!(
            self,
            "store.participants",
            |s| s.participants(id),
            json!({"id": id})
        )
    }

    pub async fn add_participant(&self, id: &str, account: &str, added_by: &str) -> Result<()> {
        delegate!(
            self,
            "store.add_participant",
            |s| s.add_participant(id, account, added_by),
            json!({"id": id, "account": account, "added_by": added_by})
        )
    }

    /// Removes a participant. `by` is who is asking: the owner may remove
    /// anyone, and anyone may remove themselves.
    pub async fn remove_participant(&self, id: &str, account: &str, by: &str) -> Result<bool> {
        delegate!(
            self,
            "store.remove_participant",
            |s| s.remove_participant(id, account),
            json!({"id": id, "account": account, "by": by})
        )
    }

    pub async fn sessions_participating(&self, account: &str) -> Result<Vec<String>> {
        delegate!(
            self,
            "store.sessions_participating",
            |s| s.sessions_participating(account),
            json!({"account": account})
        )
    }

    pub async fn get_user_spend(&self, user: &str) -> Result<f64> {
        delegate!(
            self,
            "store.get_user_spend",
            |s| s.get_user_spend(user),
            json!({"user": user})
        )
    }

    pub async fn add_user_spend(&self, user: &str, usd: f64) -> Result<f64> {
        delegate!(
            self,
            "store.add_user_spend",
            |s| s.add_user_spend(user, usd),
            json!({"user": user, "usd": usd})
        )
    }

    pub async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>> {
        delegate!(
            self,
            "store.get_session",
            |s| s.get_session(id),
            json!({ "id": id })
        )
    }

    pub async fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionMeta>> {
        delegate!(
            self,
            "store.list_sessions",
            |s| s.list_sessions(include_archived),
            json!({ "include_archived": include_archived })
        )
    }

    pub async fn rename_session(&self, id: &str, title: &str) -> Result<SessionMeta> {
        delegate!(
            self,
            "store.rename_session",
            |s| s.rename_session(id, title),
            json!({ "id": id, "title": title })
        )
    }

    pub async fn archive_session(&self, id: &str, archived: bool) -> Result<SessionMeta> {
        delegate!(
            self,
            "store.archive_session",
            |s| s.archive_session(id, archived),
            json!({ "id": id, "archived": archived })
        )
    }

    pub async fn set_mode(&self, id: &str, mode: &str) -> Result<SessionMeta> {
        delegate!(
            self,
            "store.set_mode",
            |s| s.set_mode(id, mode),
            json!({ "id": id, "mode": mode })
        )
    }

    pub async fn set_model(&self, id: &str, model: &str) -> Result<SessionMeta> {
        delegate!(
            self,
            "store.set_model",
            |s| s.set_model(id, model),
            json!({ "id": id, "model": model })
        )
    }

    pub async fn clear_resume_attempts(&self, session_id: &str) -> Result<()> {
        delegate!(
            self,
            "store.clear_resume_attempts",
            |s| s.clear_resume_attempts(session_id),
            json!({ "session": session_id })
        )
    }

    /// Marks the next interruption of this session as one we asked for.
    pub async fn expect_restart(&self, session_id: &str) -> Result<()> {
        delegate!(
            self,
            "store.expect_restart",
            |s| s.expect_restart(session_id),
            json!({ "session": session_id })
        )
    }

    pub async fn set_no_resume(&self, session_id: &str, no_resume: bool) -> Result<()> {
        delegate!(
            self,
            "store.set_no_resume",
            |s| s.set_no_resume(session_id, no_resume),
            json!({ "session": session_id, "no_resume": no_resume })
        )
    }

    // --- kv / spend -----------------------------------------------------------

    pub async fn kv_get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        delegate!(
            self,
            "store.kv_get",
            |s| s.kv_get(scope, key),
            json!({ "scope": scope, "key": key })
        )
    }

    pub async fn kv_put(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        delegate!(
            self,
            "store.kv_put",
            |s| s.kv_put(scope, key, value),
            json!({ "scope": scope, "key": key, "value": value })
        )
    }

    /// Writes a key only if it currently holds `expected`, reporting whether it
    /// did. One serialized transaction in the store, so a caller can use it to
    /// claim a state transition exactly once. See [`Store::kv_swap`].
    pub async fn kv_swap(
        &self,
        scope: &str,
        key: &str,
        expected: &str,
        value: &str,
    ) -> Result<bool> {
        delegate!(
            self,
            "store.kv_swap",
            |s| s.kv_swap(scope, key, expected, value),
            json!({ "scope": scope, "key": key, "expected": expected, "value": value })
        )
    }

    pub async fn get_spend(&self, session_id: &str) -> Result<f64> {
        delegate!(
            self,
            "store.get_spend",
            |s| s.get_spend(session_id),
            json!({ "session": session_id })
        )
    }

    pub async fn add_spend(&self, session_id: &str, usd: f64) -> Result<f64> {
        delegate!(
            self,
            "store.add_spend",
            |s| s.add_spend(session_id, usd),
            json!({ "session": session_id, "usd": usd })
        )
    }

    /// Live progress for several sessions at once.
    ///
    /// Batched because the caller is always rendering a child list, and one
    /// round trip per child turned a status render into twenty.
    pub async fn session_progress(&self, session_ids: &[String]) -> Result<Vec<SessionProgress>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        delegate!(
            self,
            "store.session_progress",
            |s| session_ids
                .iter()
                .map(|id| s.session_progress(id))
                .collect::<Result<Vec<_>>>(),
            json!({ "sessions": session_ids })
        )
    }

    // --- sub-agents -----------------------------------------------------------

    pub async fn register_subagent(
        &self,
        parent_id: &str,
        child_id: &str,
        label: &str,
        task: &str,
        agent_aspect: &str,
        model: &str,
        mode: &str,
        max_children: usize,
        ceiling: Option<&crate::policy::EffectivePolicy>,
    ) -> Result<SubagentRow> {
        match self {
            Persist::Local(store) => crate::offload::blocking(|| {
                let row = crate::subagents::Subagents::new(store).register(
                    parent_id,
                    child_id,
                    label,
                    task,
                    agent_aspect,
                    model,
                    mode,
                    max_children,
                )?;
                if let Some(policy) = ceiling {
                    store.set_ceiling(child_id, policy)?;
                }
                Ok(row)
            }),
            Persist::Remote(peer) => {
                peer.call_as(
                    "store.register_subagent",
                    json!({
                        "session": parent_id,
                        "child": child_id,
                        "label": label,
                        "task": task,
                        "agent": agent_aspect,
                        "model": model,
                        "mode": mode,
                        "max_children": max_children,
                        "ceiling": ceiling,
                    }),
                )
                .await
            }
        }
    }

    pub async fn get_subagent(&self, child_id: &str) -> Result<Option<SubagentRow>> {
        delegate!(
            self,
            "store.get_subagent",
            |s| s.get_subagent(child_id),
            json!({ "child": child_id })
        )
    }

    pub async fn subagents_of(&self, parent_id: &str) -> Result<Vec<SubagentRow>> {
        delegate!(
            self,
            "store.subagents_of",
            |s| s.subagents_of(parent_id),
            json!({ "session": parent_id })
        )
    }

    pub async fn settle_subagent(
        &self,
        child_id: &str,
        result: &str,
        cost_usd: f64,
        stopped_by: &str,
    ) -> Result<SubagentRow> {
        delegate!(
            self,
            "store.settle_subagent",
            |s| crate::subagents::Subagents::new(s).settle(child_id, result, cost_usd, stopped_by),
            json!({
                "child": child_id,
                "result": result,
                "cost_usd": cost_usd,
                "stopped_by": stopped_by,
            })
        )
    }

    pub async fn cancel_subagent(&self, child_id: &str) -> Result<SubagentRow> {
        delegate!(
            self,
            "store.cancel_subagent",
            |s| crate::subagents::Subagents::new(s).mark_cancelled(child_id),
            json!({ "child": child_id })
        )
    }

    // --- transcripts ----------------------------------------------------------
    //
    // Read-only recall across every conversation. See `transcripts.rs` for why
    // these are not pinned to the caller's own session the way `store.events`
    // is, and `serve_store_call` below for the arms that serve them.

    pub async fn conversations(
        &self,
        include_archived: bool,
        include_subagents: bool,
        limit: usize,
    ) -> Result<Vec<ConversationSummary>> {
        delegate!(
            self,
            "store.conversations",
            |s| crate::transcripts::Transcripts::new(s).conversations(
                include_archived,
                include_subagents,
                limit
            ),
            json!({
                "include_archived": include_archived,
                "include_subagents": include_subagents,
                "limit": limit,
            })
        )
    }

    pub async fn conversation_subagents(&self, root_id: &str) -> Result<Vec<ConversationSummary>> {
        delegate!(
            self,
            "store.conversation_subagents",
            |s| crate::transcripts::Transcripts::new(s).subagents(root_id),
            json!({ "root": root_id })
        )
    }

    pub async fn read_transcript(
        &self,
        session_id: &str,
        from_seq: u64,
        limit: usize,
        max_chars: usize,
    ) -> Result<Vec<TranscriptEntry>> {
        delegate!(
            self,
            "store.read_transcript",
            |s| crate::transcripts::Transcripts::new(s)
                .read(session_id, from_seq, limit, max_chars),
            json!({
                "id": session_id,
                "from_seq": from_seq,
                "limit": limit,
                "max_chars": max_chars,
            })
        )
    }

    pub async fn search_transcripts(&self, query: &SearchQuery) -> Result<SearchReport> {
        delegate!(
            self,
            "store.search_transcripts",
            |s| crate::transcripts::Transcripts::new(s).search(query),
            json!({ "query": query })
        )
    }

    // --- skill vectors ----------------------------------------------------------

    pub async fn skill_vector(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Persist::Local(s) => Ok(s.skill_vector(key)),
            Persist::Remote(peer) => {
                peer.call_as("store.skill_vector", json!({ "key": key }))
                    .await
            }
        }
    }

    pub async fn put_skill_vector(&self, key: &str, vector: &[u8]) -> Result<()> {
        delegate!(
            self,
            "store.put_skill_vector",
            |s| s.put_skill_vector(key, vector),
            json!({ "key": key, "vector": vector })
        )
    }

    pub async fn retain_skill_vectors(&self, keep: &[String]) -> Result<usize> {
        delegate!(
            self,
            "store.retain_skill_vectors",
            |s| s.retain_skill_vectors(keep),
            json!({ "keep": keep })
        )
    }

    // --- legacy revision registry (read-only) --------------------------------

    pub async fn list_revisions(&self, aspect_key: &str) -> Result<Vec<Value>> {
        delegate!(
            self,
            "store.list_revisions",
            |s| s.list_revisions(aspect_key),
            json!({ "aspect": aspect_key })
        )
    }
}

/// The gateway's answer to one `store.*` request from a worker. Lives here so
/// the method names and their local counterparts stay side by side.
///
/// `caller_session` is the session the requesting worker was spawned for. A
/// worker runs an agent-modified kernel and is therefore untrusted: the
/// methods that touch a conversation's own transcript, spend, and resume state
/// are pinned to that session, so a buggy or hostile worker cannot forge
/// events into, drain the budget of, or unstick another conversation. The
/// session-management and shared-store methods (create/list/get/rename/
/// archive/kv/skill-vector) stay open, because the agent legitimately manages
/// other conversations and shared state through them.
///
/// The `store.conversations` / `read_transcript` / `search_transcripts` /
/// `conversation_subagents` arms are open too, and are the one place where a
/// worker reads a conversation that is not its own. They are read-only by
/// construction — see `crate::transcripts` — which is what separates them from
/// the arms above.
pub async fn serve_store_call(
    store: &Store,
    cfg: Option<&crate::config::Config>,
    method: &str,
    params: Value,
    caller_session: &str,
) -> Result<Value> {
    // Every arm below is a synchronous redb call, served on the gateway for a
    // worker that is waiting on a 60s RPC.
    crate::offload::blocking(|| serve_store_call_inner(store, cfg, method, params, caller_session))
}

fn serve_store_call_inner(
    store: &Store,
    cfg: Option<&crate::config::Config>,
    method: &str,
    params: Value,
    caller_session: &str,
) -> Result<Value> {
    fn get_str<'v>(params: &'v Value, key: &str) -> Result<&'v str> {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing '{key}'"))
    }
    fn to_value<T: serde::Serialize>(value: T) -> Result<Value> {
        Ok(serde_json::to_value(value)?)
    }
    // The session-private methods: their `session` argument must be the
    // caller's own, or one of the sub-agents the caller spawned. An empty
    // `caller_session` means the call did not come from a session-bound worker
    // (the local test grip), so the check is skipped.
    //
    // Children are admitted because a sub-agent runs *inside* its parent's
    // worker: that worker legitimately appends to the child's log, reads it
    // back, and accounts for its spend. The boundary the check exists to
    // defend — one worker cannot touch an unrelated conversation — is
    // unchanged, because a child's root is resolved from the registry here on
    // the gateway rather than from anything the worker says.
    fn own_session<'v>(store: &Store, params: &'v Value, caller: &str) -> Result<&'v str> {
        let session = get_str(params, "session")?;
        if caller.is_empty() || session == caller {
            return Ok(session);
        }
        // A sub-agent of this caller. `root_id` comes from the gateway's own
        // registry, so a worker cannot claim kinship it does not have.
        if let Ok(Some(row)) = store.get_subagent(session) {
            if row.root_id == caller {
                return Ok(session);
            }
        }
        anyhow::bail!("a worker may only act on its own session or one of its sub-agents")
    }

    fn own_owner<'v>(store: &Store, id: &'v str, owner: Option<&str>) -> Result<&'v str> {
        if let Some(owner) = owner {
            anyhow::ensure!(
                store.owner_of_root(id)?.as_deref() == Some(owner),
                "conversation belongs to another user"
            );
        }
        Ok(id)
    }

    fn own_scope<'v>(store: &Store, scope: &'v str, owner: Option<&str>) -> Result<&'v str> {
        let Some(owner) = owner else {
            return Ok(scope);
        };
        if scope == "global" || scope == format!("user:{owner}") {
            return Ok(scope);
        }
        anyhow::ensure!(
            !scope.starts_with("user:"),
            "user settings belong to another user"
        );
        own_owner(store, scope, Some(owner))
    }

    let caller_owner = if caller_session.is_empty() {
        None
    } else {
        store.owner_of_root(caller_session)?
    };
    let transcripts = || crate::transcripts::Transcripts::owned(store, caller_owner.as_deref());

    match method {
        "store.append_event" => {
            let session = own_session(store, &params, caller_session)?;
            let event: SessionEvent =
                serde_json::from_value(params.get("event").cloned().unwrap_or(Value::Null))?;
            to_value(store.append_event(session, event)?)
        }
        "store.events" => {
            let session = own_session(store, &params, caller_session)?;
            let from = params.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
            to_value(store.events(session, from)?)
        }
        "store.create_session" => {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string);
            let requested = params
                .get("owner")
                .and_then(Value::as_str)
                .unwrap_or("local");
            let owner = caller_owner.as_deref().unwrap_or(requested);
            to_value(store.create_session(title, get_str(&params, "mode")?, owner)?)
        }
        "store.get_session" => {
            let id = get_str(&params, "id")?;
            if let Some(owner) = caller_owner.as_deref() {
                anyhow::ensure!(
                    store.owner_of_root(id)?.as_deref() == Some(owner),
                    "conversation belongs to another user"
                );
            }
            to_value(store.get_session(id)?)
        }
        "store.owner_of_root" => to_value(store.owner_of_root(get_str(&params, "id")?)?),
        "store.session_policy" => {
            let id = get_str(&params, "id")?;
            let scoped = serde_json::json!({ "session": id });
            own_session(store, &scoped, caller_session)?;
            let owner = store
                .owner_of_root(id)?
                .ok_or_else(|| anyhow::anyhow!("conversation has no owner"))?;
            let cfg = cfg.ok_or_else(|| anyhow::anyhow!("session policy needs gateway config"))?;

            // Whoever is speaking, not whoever owns the conversation. A
            // participant brings their own authority and nothing more; an
            // unidentified speaker falls back to the owner, which is what this
            // did before there were participants at all.
            let speaker = params
                .get("speaker")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(owner.as_str());

            // A speaker who is neither the owner nor a participant gets
            // nothing. Without this, naming any account id in the frame would
            // borrow its policy.
            let known = speaker == owner || store.is_participant(id, speaker)?;
            if !known {
                anyhow::bail!("{speaker} is not a participant in this conversation");
            }

            let mut policy = cfg.auth.policy_for(speaker).as_ref().clone();

            // The conversation's ceiling is the other half of the rule. Absent
            // for anything created before ceilings existed, and then the
            // speaker's own policy stands alone — today's behaviour exactly.
            if let Some(ceiling) = store.ceiling_of(id)? {
                policy = policy.intersect(&ceiling);
            }
            to_value(&policy)
        }
        "store.ceiling_of" => to_value(store.ceiling_of(get_str(&params, "id")?)?),
        "store.set_ceiling" => {
            // A worker must never set any conversation ceiling. Delegation
            // stamps a child atomically in `store.register_subagent`; ordinary
            // session and fork creation happen in the trusted gateway.
            anyhow::ensure!(
                caller_session.is_empty(),
                "a conversation cannot set its own ceiling"
            );
            let policy: crate::policy::EffectivePolicy = serde_json::from_value(
                params
                    .get("policy")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("policy is required"))?,
            )?;
            to_value(store.set_ceiling(get_str(&params, "id")?, &policy)?)
        }
        "store.is_participant" => to_value(
            store.is_participant(get_str(&params, "id")?, get_str(&params, "account")?)?,
        ),
        "store.participants" => {
            let id = get_str(&params, "id")?;
            let scoped = serde_json::json!({ "session": id });
            own_session(store, &scoped, caller_session)?;
            to_value(store.participants(id)?)
        }
        "store.add_participant" => {
            // Only the owner invites, and only from the gateway: a turn that
            // could add participants could widen who may prompt it.
            anyhow::ensure!(
                caller_session.is_empty(),
                "a conversation cannot change its own participants"
            );
            let id = get_str(&params, "id")?;
            let added_by = get_str(&params, "added_by")?;
            anyhow::ensure!(
                store.owner_of_root(id)?.as_deref() == Some(added_by),
                "only the owner may invite"
            );
            to_value(store.add_participant(id, get_str(&params, "account")?, added_by)?)
        }
        "store.remove_participant" => {
            anyhow::ensure!(
                caller_session.is_empty(),
                "a conversation cannot change its own participants"
            );
            let id = get_str(&params, "id")?;
            let by = get_str(&params, "by")?;
            let account = get_str(&params, "account")?;
            // Symmetric with `add_participant`, which is the point: guarding
            // only the invite would let any participant remove the others, or
            // remove a co-participant to hide what they had seen. The one
            // exception is leaving voluntarily, which needs nobody's
            // permission.
            anyhow::ensure!(
                store.owner_of_root(id)?.as_deref() == Some(by) || by == account,
                "only the owner may remove a participant"
            );
            to_value(store.remove_participant(id, account)?)
        }
        "store.sessions_participating" => {
            let account = get_str(&params, "account")?;
            if let Some(caller) = caller_owner.as_deref() {
                anyhow::ensure!(
                    account == caller,
                    "cannot list another user's conversations"
                );
            }
            to_value(store.sessions_participating(account)?)
        }
        "store.get_user_spend" => {
            let owner = caller_owner.as_deref().unwrap_or(get_str(&params, "user")?);
            if caller_owner.is_some() {
                anyhow::ensure!(
                    get_str(&params, "user")? == owner,
                    "cannot read another user's spend"
                );
            }
            to_value(store.get_user_spend(owner)?)
        }
        "store.add_user_spend" => {
            let owner = caller_owner.as_deref().unwrap_or(get_str(&params, "user")?);
            if caller_owner.is_some() {
                anyhow::ensure!(
                    get_str(&params, "user")? == owner,
                    "cannot write another user's spend"
                );
            }
            let usd = params.get("usd").and_then(Value::as_f64).unwrap_or(0.0);
            to_value(store.add_user_spend(owner, usd)?)
        }
        "store.list_sessions_owned" => {
            let include = params
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let requested = params.get("owner").and_then(Value::as_str);
            if let Some(caller) = caller_owner.as_deref() {
                anyhow::ensure!(
                    requested == Some(caller),
                    "cannot list another user's sessions"
                );
            }
            to_value(store.list_sessions_owned(requested, include)?)
        }
        "store.list_sessions" => {
            let include = params
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            to_value(store.list_sessions_owned(caller_owner.as_deref(), include)?)
        }
        "store.rename_session" => {
            let id = own_owner(store, get_str(&params, "id")?, caller_owner.as_deref())?;
            to_value(store.rename_session(id, get_str(&params, "title")?)?)
        }
        "store.archive_session" => {
            let id = own_owner(store, get_str(&params, "id")?, caller_owner.as_deref())?;
            let archived = params
                .get("archived")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            to_value(store.archive_session(id, archived)?)
        }
        "store.set_mode" => {
            let id = own_owner(store, get_str(&params, "id")?, caller_owner.as_deref())?;
            let mode = get_str(&params, "mode")?;
            // H1. `store.set_mode` itself validates nothing — it truncates to
            // 32 characters and writes whatever it is given — and this arm is
            // reachable from the browser's `set-mode` frame. An unknown mode
            // used to be accepted silently, which mattered because
            // `agent-core`'s `read_only(mode)` treats a mode it has never heard
            // of as *unrestricted*. Writing "agnet" was therefore a way to
            // widen a conversation's tool surface by typo.
            //
            // Since step 2 the mode is no longer what bounds a conversation —
            // the stored ceiling is, and it is checked in `host_api::require`
            // whatever the mode says — so this is defence in depth. It is worth
            // having anyway: a mode nothing recognises produces a conversation
            // whose tool list is decided by a fallback rather than by anyone's
            // intent, and the honest answer to that is to refuse the write.
            //
            // The check lives here rather than in `Store` because the store
            // holds no configuration; `cfg` is present only on the gateway
            // side, which is also the only side this arm is served from.
            //
            // An empty mode is allowed: it is not an unrecognised mode but the
            // absence of one, which is how most conversations run and the only
            // way to clear a mode once set.
            if let Some(cfg) = cfg {
                anyhow::ensure!(
                    mode.is_empty() || cfg.mode(mode).is_some(),
                    "unknown mode: {mode}. An unrecognised mode would leave the \
                     tool surface to a fallback rather than to a decision."
                );
            }
            to_value(store.set_mode(id, mode)?)
        }
        "store.set_model" => {
            let id = own_owner(store, get_str(&params, "id")?, caller_owner.as_deref())?;
            to_value(store.set_model(id, get_str(&params, "model")?)?)
        }
        "store.clear_resume_attempts" => {
            to_value(store.clear_resume_attempts(own_session(store, &params, caller_session)?)?)
        }
        "store.expect_restart" => {
            to_value(store.expect_restart(own_session(store, &params, caller_session)?)?)
        }
        "store.set_no_resume" => {
            let flag = params
                .get("no_resume")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            to_value(store.set_no_resume(own_session(store, &params, caller_session)?, flag)?)
        }
        "store.kv_get" => {
            let scope = own_scope(store, get_str(&params, "scope")?, caller_owner.as_deref())?;
            to_value(store.kv_get(scope, get_str(&params, "key")?)?)
        }
        "store.kv_put" => {
            let scope = own_scope(store, get_str(&params, "scope")?, caller_owner.as_deref())?;
            to_value(store.kv_put(scope, get_str(&params, "key")?, get_str(&params, "value")?)?)
        }
        "store.kv_swap" => {
            let scope = own_scope(store, get_str(&params, "scope")?, caller_owner.as_deref())?;
            to_value(store.kv_swap(
                scope,
                get_str(&params, "key")?,
                get_str(&params, "expected")?,
                get_str(&params, "value")?,
            )?)
        }
        "store.get_spend" => {
            to_value(store.get_spend(own_session(store, &params, caller_session)?)?)
        }
        // Scoped per id rather than in bulk: the batch exists to save round
        // trips, not to widen what a worker may look at.
        "store.session_progress" => {
            let ids: Vec<String> =
                serde_json::from_value(params.get("sessions").cloned().unwrap_or(Value::Null))?;
            let mut out = Vec::with_capacity(ids.len());
            for id in &ids {
                let scoped = json!({ "session": id });
                let checked = own_session(store, &scoped, caller_session)?.to_string();
                out.push(store.session_progress(&checked)?);
            }
            to_value(out)
        }
        "store.add_spend" => {
            let usd = params.get("usd").and_then(Value::as_f64).unwrap_or(0.0);
            to_value(store.add_spend(own_session(store, &params, caller_session)?, usd)?)
        }
        // Sub-agents. The parent is pinned to the caller on register, so a
        // worker cannot graft a child onto somebody else's conversation; the
        // rest key off a child id that only exists because a register call
        // already passed that check.
        "store.register_subagent" => {
            let parent = own_session(store, &params, caller_session)?;
            let max = params
                .get("max_children")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let child_id = get_str(&params, "child")?;
            let row = crate::subagents::Subagents::new(store).register(
                parent,
                child_id,
                params.get("label").and_then(Value::as_str).unwrap_or(""),
                params.get("task").and_then(Value::as_str).unwrap_or(""),
                params.get("agent").and_then(Value::as_str).unwrap_or(""),
                params.get("model").and_then(Value::as_str).unwrap_or(""),
                params
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("agent"),
                max,
            )?;
            if let Some(value) = params.get("ceiling").filter(|v| !v.is_null()) {
                let ceiling: crate::policy::EffectivePolicy =
                    serde_json::from_value(value.clone())?;
                store.set_ceiling(child_id, &ceiling)?;
            }
            to_value(row)
        }
        "store.get_subagent" => to_value(store.get_subagent(get_str(&params, "child")?)?),
        "store.subagents_of" => {
            to_value(store.subagents_of(own_session(store, &params, caller_session)?)?)
        }
        "store.settle_subagent" => {
            let cost = params
                .get("cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            to_value(
                crate::subagents::Subagents::new(store).settle(
                    get_str(&params, "child")?,
                    params.get("result").and_then(Value::as_str).unwrap_or(""),
                    cost,
                    params
                        .get("stopped_by")
                        .and_then(Value::as_str)
                        .unwrap_or("stop"),
                )?,
            )
        }
        "store.cancel_subagent" => to_value(
            crate::subagents::Subagents::new(store).mark_cancelled(get_str(&params, "child")?)?,
        ),
        // Transcript recall. These four are the *only* arms that name a session
        // and deliberately skip `own_session`, and the exemption is the feature
        // rather than an oversight: an agent asking "have I solved this before"
        // or "what did that sub-agent actually find" has to read logs its own
        // session did not write.
        //
        // What makes that safe to grant is that none of them can write. They
        // route to `crate::transcripts`, which holds no write path at all — a
        // property pinned by a test in that module, because the read was widened
        // on precisely that promise. The mutating arms above keep `own_session`
        // untouched, so a worker still cannot forge an event into another
        // conversation, drain its budget or unstick its turn.
        "store.conversations" => {
            let include_archived = params
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let include_subagents = params
                .get("include_subagents")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
            to_value(transcripts().conversations(include_archived, include_subagents, limit)?)
        }
        "store.conversation_subagents" => {
            to_value(transcripts().subagents(get_str(&params, "root")?)?)
        }
        "store.read_transcript" => {
            let from = params.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
            let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max_chars = params.get("max_chars").and_then(Value::as_u64).unwrap_or(0) as usize;
            to_value(transcripts().read(get_str(&params, "id")?, from, limit, max_chars)?)
        }
        "store.search_transcripts" => {
            let query: crate::transcripts::SearchQuery =
                serde_json::from_value(params.get("query").cloned().unwrap_or(Value::Null))?;
            to_value(transcripts().search(&query)?)
        }
        "store.skill_vector" => to_value(store.skill_vector(get_str(&params, "key")?)),
        "store.put_skill_vector" => {
            let vector: Vec<u8> =
                serde_json::from_value(params.get("vector").cloned().unwrap_or(Value::Null))?;
            to_value(store.put_skill_vector(get_str(&params, "key")?, &vector)?)
        }
        "store.retain_skill_vectors" => {
            let keep: Vec<String> =
                serde_json::from_value(params.get("keep").cloned().unwrap_or(Value::Null))?;
            to_value(store.retain_skill_vectors(&keep)?)
        }
        "store.list_revisions" => {
            to_value(store.list_revisions::<Value>(get_str(&params, "aspect")?)?)
        }
        other => anyhow::bail!("unknown store method {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{self, Handler};
    use std::pin::Pin;
    use tempfile::TempDir;
    use tokio::net::UnixStream;

    struct GatewaySide(Arc<Store>);
    impl Handler for GatewaySide {
        fn handle(
            self: Arc<Self>,
            method: String,
            params: Value,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
            let store = self.0.clone();
            Box::pin(async move {
                if method == "hello" {
                    return Ok(ipc::hello_response());
                }
                // The test grip is not a session-bound worker; an empty
                // caller skips the own-session check.
                serve_store_call(&store, None, &method, params, "").await
            })
        }
        fn handle_note(self: Arc<Self>, _name: String, _params: Value) {}
    }

    /// The gateway side with real configuration behind it, which is what
    /// `store.session_policy` needs: policy is resolved from `[[users]]` and
    /// `[[roles]]`, and the whole point of the arm is that it happens on the
    /// gateway rather than in a worker's rewritable checkout.
    struct GatewayWithCfg(Arc<Store>, Arc<crate::config::Config>, String);
    impl Handler for GatewayWithCfg {
        fn handle(
            self: Arc<Self>,
            method: String,
            params: Value,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
            let store = self.0.clone();
            let cfg = self.1.clone();
            let caller = self.2.clone();
            Box::pin(async move {
                if method == "hello" {
                    return Ok(ipc::hello_response());
                }
                serve_store_call(&store, Some(&cfg), &method, params, &caller).await
            })
        }
        fn handle_note(self: Arc<Self>, _name: String, _params: Value) {}
    }

    /// Two accounts: one that can change things, one that cannot.
    fn two_account_config() -> Arc<crate::config::Config> {
        use crate::policy::Cap;
        let mut cfg = crate::config::Config::load().unwrap();

        let mut writer = cfg.auth.local_policy.as_ref().clone();
        writer.admin = true;
        writer.read_only = false;

        let mut reader = cfg.auth.local_policy.as_ref().clone();
        reader.admin = false;
        reader.read_only = true;
        reader.denied.insert(Cap::Delegation);

        cfg.auth.users_mode = true;
        cfg.auth.users = vec![
            crate::config::UserSpec {
                id: "writer".into(),
                name: "Writer".into(),
                role: "admin".into(),
                password_hash: crate::config::Secret::new(""),
                discord_id: String::new(),
                policy: Arc::new(writer),
            },
            crate::config::UserSpec {
                id: "reader".into(),
                name: "Reader".into(),
                role: "reader".into(),
                password_hash: crate::config::Secret::new(""),
                discord_id: String::new(),
                policy: Arc::new(reader),
            },
        ];
        Arc::new(cfg)
    }

    /// The rule the whole multi-user design rests on, asserted across the IPC
    /// boundary because that is the only path a worker ever uses:
    ///
    ///     effective(turn) = policy(speaker) ∩ ceiling(session)
    ///
    /// A `Persist::Local` test would prove nothing here — `session_policy`
    /// deliberately refuses the local arm, since a worker must not resolve
    /// policy from configuration in its own rewritable checkout.
    #[tokio::test]
    async fn a_turns_authority_comes_from_the_speaker_not_the_owner() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());
        let cfg = two_account_config();

        // A conversation owned by the account that *can* change things.
        let convo = store
            .create_session(Some("writer's chat".into()), "agent", "writer")
            .unwrap();
        // The read-only account is invited in.
        store
            .add_participant(&convo.id, "reader", "writer")
            .unwrap();

        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(
            gw_stream,
            Arc::new(GatewayWithCfg(store.clone(), cfg.clone(), convo.id.clone())),
        );
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        // The owner speaking gets their own authority.
        let owner_turn = remote
            .session_policy(&convo.id, Some("writer"))
            .await
            .unwrap();
        assert!(!owner_turn.read_only, "the owner can still write");
        assert!(!owner_turn.denies(crate::policy::Cap::FilesystemWrite));

        // The invited read-only account speaking in that same conversation
        // gets *their* authority, not the owner's. This is the question this
        // whole design was built to answer.
        let guest_turn = remote
            .session_policy(&convo.id, Some("reader"))
            .await
            .unwrap();
        assert!(
            guest_turn.read_only,
            "a read-only participant must not inherit the owner's write access"
        );
        assert!(guest_turn.denies(crate::policy::Cap::FilesystemWrite));
        assert!(guest_turn.denies(crate::policy::Cap::Devkit));
        assert!(!guest_turn.admin);

        // Someone who was never invited borrows nothing by naming themselves.
        assert!(
            remote
                .session_policy(&convo.id, Some("stranger"))
                .await
                .is_err(),
            "a non-participant must be refused, not resolved"
        );

        // No speaker falls back to the owner, which is what a resume after a
        // restart does and what every turn did before this existed.
        let resumed = remote.session_policy(&convo.id, None).await.unwrap();
        assert!(!resumed.read_only);
    }

    /// The other half of the rule: a conversation's ceiling narrows *everyone*,
    /// including an admin. This is what makes the Discord guarantee hard rather
    /// than a tool filter in a component the agent can rewrite.
    #[tokio::test]
    async fn a_ceiling_narrows_even_an_admin() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());
        let cfg = two_account_config();

        let convo = store
            .create_session(Some("from discord".into()), "chat", "writer")
            .unwrap();

        // A Discord-flavoured ceiling: read-only, no delegation.
        let mut ceiling = cfg.auth.local_policy.as_ref().clone();
        ceiling.admin = false;
        ceiling.read_only = true;
        ceiling.denied.insert(crate::policy::Cap::Delegation);
        store.set_ceiling(&convo.id, &ceiling).unwrap();

        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(
            gw_stream,
            Arc::new(GatewayWithCfg(store.clone(), cfg.clone(), convo.id.clone())),
        );
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        // The admin owner speaking under that ceiling is read-only anyway.
        let turn = remote
            .session_policy(&convo.id, Some("writer"))
            .await
            .unwrap();
        assert!(turn.read_only, "the ceiling binds the owner too");
        assert!(!turn.admin, "and takes admin away with it");
        assert!(turn.denies(crate::policy::Cap::Devkit));
        assert!(turn.denies(crate::policy::Cap::Delegation));

        // Reading is untouched: a ceiling narrows, it does not disable.
        assert!(!turn.denies(crate::policy::Cap::FilesystemRead));
    }

    /// A conversation must not be able to raise its own ceiling, and a worker
    /// is the only caller that arrives scoped to one.
    #[tokio::test]
    async fn a_conversation_cannot_raise_its_own_ceiling() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());
        let cfg = two_account_config();
        let convo = store
            .create_session(None, "chat", "writer")
            .unwrap();

        let mut narrow = cfg.auth.local_policy.as_ref().clone();
        narrow.read_only = true;
        store.set_ceiling(&convo.id, &narrow).unwrap();

        // A session-bound caller: exactly how a worker's IPC arrives.
        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(
            gw_stream,
            Arc::new(GatewayWithCfg(store.clone(), cfg.clone(), convo.id.clone())),
        );
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        let mut wide = cfg.auth.local_policy.as_ref().clone();
        wide.admin = true;
        wide.read_only = false;
        assert!(
            remote.set_ceiling(&convo.id, &wide).await.is_err(),
            "a turn that could widen its own ceiling would not have one"
        );
        assert!(
            remote
                .add_participant(&convo.id, "reader", "writer")
                .await
                .is_err(),
            "nor may it invite someone who could then prompt it"
        );

        // And the stored ceiling is untouched.
        assert!(store.ceiling_of(&convo.id).unwrap().unwrap().read_only);

        // Delegation stamps the child's ceiling as part of registration. The
        // separate set-ceiling RPC remains forbidden to every worker.
        let child = store.create_session(None, "plan", "writer").unwrap();
        remote
            .register_subagent(
                &convo.id,
                &child.id,
                "review",
                "Review the guide as a novice and report confusing parts.",
                "scout",
                "test-model",
                "plan",
                8,
                Some(&narrow),
            )
            .await
            .unwrap();
        assert!(store.ceiling_of(&child.id).unwrap().unwrap().read_only);

        let unrelated = store.create_session(None, "plan", "writer").unwrap();
        assert!(
            remote.set_ceiling(&unrelated.id, &narrow).await.is_err(),
            "a worker cannot use the separate RPC on any conversation"
        );
    }

    /// Removal is guarded the same way inviting is.
    ///
    /// Guarding only the invite is the plausible mistake, and it is a real
    /// hole: any participant could then evict the others, or evict a
    /// co-participant to hide what they had been shown. The single exception
    /// is leaving, which needs nobody's permission.
    #[tokio::test]
    async fn only_the_owner_removes_someone_else_but_anyone_may_leave() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());
        let cfg = two_account_config();
        let convo = store
            .create_session(None, "agent", "writer")
            .unwrap();
        store.add_participant(&convo.id, "reader", "writer").unwrap();

        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        // Unscoped: the gateway acting for a browser, which is the only way a
        // participant change ever arrives.
        let (_gw_peer, gw_done) = ipc::Peer::spawn(
            gw_stream,
            Arc::new(GatewayWithCfg(store.clone(), cfg.clone(), String::new())),
        );
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        // A participant cannot evict a fellow guest…
        store.add_participant(&convo.id, "other", "writer").unwrap();
        assert!(
            remote
                .remove_participant(&convo.id, "other", "reader")
                .await
                .is_err(),
            "a guest must not be able to remove another guest"
        );
        assert!(store.is_participant(&convo.id, "other").unwrap());

        // …but may remove themselves.
        assert!(remote
            .remove_participant(&convo.id, "reader", "reader")
            .await
            .unwrap());
        assert!(!store.is_participant(&convo.id, "reader").unwrap());

        // And the owner may remove anyone.
        assert!(remote
            .remove_participant(&convo.id, "other", "writer")
            .await
            .unwrap());
        assert!(!store.is_participant(&convo.id, "other").unwrap());
    }

    /// The gateway side as a *session-bound* worker sees it: every call is
    /// pinned to `caller_session`, exactly as `roles::gateway` serves a
    /// H1. The browser's `set-mode` frame reaches `store.set_mode`, and the
    /// store validates nothing — it truncates to 32 characters and writes
    /// whatever it is handed. An unknown mode used to be accepted silently,
    /// which mattered because `agent-core` treated a mode it had never heard of
    /// as unrestricted: `set-mode "agnet"` was a way to widen a conversation by
    /// typo.
    ///
    /// Asserted across the wire because that is the only path the frame takes.
    #[tokio::test]
    async fn a_mode_nobody_declared_cannot_be_written() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());
        let cfg = two_account_config();

        let convo = store
            .create_session(Some("chat".into()), "agent", "writer")
            .unwrap();

        // `set_mode` is scoped by owner, not by session, so the caller session
        // is left empty and `caller_owner` does the work.
        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(
            gw_stream,
            Arc::new(GatewayWithCfg(store.clone(), cfg.clone(), String::new())),
        );
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        // The two built-in modes are accepted.
        remote.set_mode(&convo.id, "plan").await.unwrap();
        assert_eq!(
            store.get_session(&convo.id).unwrap().unwrap().mode,
            "plan",
            "a declared mode must still be settable"
        );
        remote.set_mode(&convo.id, "agent").await.unwrap();

        // A typo is refused rather than written.
        let refused = remote.set_mode(&convo.id, "agnet").await;
        assert!(refused.is_err(), "an unknown mode must be refused");
        let complaint = format!("{:#}", refused.unwrap_err());
        assert!(
            complaint.contains("unknown mode"),
            "the refusal should say what was wrong, got: {complaint}"
        );

        // And the refusal left the conversation as it was, rather than half
        // applying — the mode that was valid a moment ago is still in place.
        assert_eq!(
            store.get_session(&convo.id).unwrap().unwrap().mode,
            "agent",
            "a refused write must not have landed"
        );

        // Clearing the mode is not the same as naming an unknown one: an empty
        // mode is the ordinary "no mode set" case and the only way back to it.
        remote.set_mode(&convo.id, "").await.unwrap();
        assert_eq!(
            store.get_session(&convo.id).unwrap().unwrap().mode,
            "",
            "it must stay possible to clear a mode"
        );
    }

    /// worker's IPC.
    struct GatewayAs(Arc<Store>, String);
    impl Handler for GatewayAs {
        fn handle(
            self: Arc<Self>,
            method: String,
            params: Value,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
            let store = self.0.clone();
            let caller = self.1.clone();
            Box::pin(async move {
                if method == "hello" {
                    return Ok(ipc::hello_response());
                }
                serve_store_call(&store, None, &method, params, &caller).await
            })
        }
        fn handle_note(self: Arc<Self>, _name: String, _params: Value) {}
    }

    struct Mute;
    impl Handler for Mute {
        fn handle(
            self: Arc<Self>,
            _method: String,
            _params: Value,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
            Box::pin(async { Ok(Value::Null) })
        }
        fn handle_note(self: Arc<Self>, _name: String, _params: Value) {}
    }

    /// The same operations must behave identically through both arms.
    #[tokio::test]
    async fn local_and_remote_agree() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());

        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(gw_stream, Arc::new(GatewaySide(store.clone())));
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);

        let local = Persist::Local(store.clone());
        let remote = Persist::Remote(wk_peer);

        // Create through the remote arm, read back through both.
        let meta = remote
            .create_session(Some("hi".into()), &"build", "local")
            .await
            .unwrap();
        assert_eq!(
            local.get_session(&meta.id).await.unwrap().unwrap().title,
            "hi"
        );
        assert_eq!(
            remote.get_session(&meta.id).await.unwrap().unwrap().mode,
            "build"
        );

        // Events round-trip with their WIT payload intact.
        let seq = remote
            .append_event(&meta.id, SessionEvent::Nudge("steer left".into()))
            .await
            .unwrap()
            .seq;
        let events = local.events(&meta.id, 0).await.unwrap();
        assert_eq!(events.last().unwrap().seq, seq);

        // kv and spend.
        remote.kv_put("global", "k", "v").await.unwrap();
        assert_eq!(local.kv_get("global", "k").await.unwrap().unwrap(), "v");
        remote.add_spend(&meta.id, 0.25).await.unwrap();
        assert_eq!(local.get_spend(&meta.id).await.unwrap(), 0.25);

        // The legacy revision registry stays readable across the wire.
        store
            .put_revision("agent", 1, &serde_json::json!({"revision": 1, "note": "x"}))
            .unwrap();
        let rows = remote.list_revisions("agent").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["note"], "x");

        // Errors cross the boundary as errors.
        assert!(remote.rename_session("nope", "t").await.is_err());
    }

    /// A worker is pinned to its session's owner, and the gateway applies that
    /// to every arm that used to be open across the whole database.
    ///
    /// This is the multi-user leak the gateway side exists to close: an agent
    /// in Alice's conversation asking for the catalogue, another session, a
    /// transcript or a search must never see Bob's. The check lives on the
    /// gateway side of the IPC precisely so a branch running a modified kernel
    /// cannot undo it — which is why it is asserted across the wire and not
    /// through the local arm.
    #[tokio::test]
    async fn a_worker_sees_only_its_owners_conversations() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());

        let alice = store
            .create_session(Some("alice's".into()), &"agent", "alice")
            .unwrap();
        let bob = store
            .create_session(Some("bob's".into()), &"agent", "bob")
            .unwrap();
        store
            .append_event(&bob.id, SessionEvent::Nudge("the zebra password".into()))
            .unwrap();
        store
            .append_event(&alice.id, SessionEvent::Nudge("nothing about zebras".into()))
            .unwrap();

        // A worker running Alice's conversation.
        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(
            gw_stream,
            Arc::new(GatewayAs(store.clone(), alice.id.clone())),
        );
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        // Listing, by either name the wire knows.
        let listed = remote.list_sessions(true).await.unwrap();
        assert_eq!(listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec![alice.id.as_str()]);
        let listed = remote.list_sessions_owned(None, true).await.unwrap();
        assert_eq!(listed.len(), 1, "asking for everyone's still gets only the owner's");
        // A worker cannot name an owner at all: the remote arm drops the
        // argument and the gateway lists for the caller's owner, so asking
        // for bob's gets alice's.
        let listed = remote.list_sessions_owned(Some("bob"), true).await.unwrap();
        assert_eq!(listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec![alice.id.as_str()]);

        // Fetching by id.
        assert!(remote.get_session(&alice.id).await.unwrap().is_some());
        assert!(remote.get_session(&bob.id).await.is_err(), "bob's is refused, not None");
        assert_eq!(remote.owner_of_root(&alice.id).await.unwrap().as_deref(), Some("alice"));

        // Recall: the catalogue, a read and a search.
        let convs = remote.conversations(true, false, 0).await.unwrap();
        assert!(convs.iter().any(|c| c.id == alice.id));
        assert!(!convs.iter().any(|c| c.id == bob.id));
        assert!(remote.read_transcript(&bob.id, 0, 0, 0).await.is_err());
        assert!(remote.conversation_subagents(&bob.id).await.is_err());
        let report = remote
            .search_transcripts(&crate::transcripts::SearchQuery {
                pattern: "zebra".into(),
                include_archived: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(report.total_matches, 1, "{report:?}");
        assert!(report.hits.iter().all(|h| h.session_id == alice.id));
        let report = remote
            .search_transcripts(&crate::transcripts::SearchQuery {
                pattern: "zebra".into(),
                session_id: bob.id.clone(),
                ..Default::default()
            })
            .await;
        assert!(
            report.map(|r| r.total_matches == 0).unwrap_or(true),
            "naming bob's conversation outright finds nothing in it"
        );

        // A session this worker creates (delegation) belongs to its owner,
        // whatever owner the params claim.
        let child = remote
            .create_session(Some("child".into()), &"agent", "bob")
            .await
            .unwrap();
        assert_eq!(store.owner_of(&child.id).unwrap().as_deref(), Some("alice"));

        // Spend: only the owner's row is reachable, either way.
        remote.add_user_spend("alice", 0.5).await.unwrap();
        assert!(remote.add_user_spend("bob", 0.5).await.is_err());
        assert!(remote.get_user_spend("bob").await.is_err());
        assert_eq!(remote.get_user_spend("alice").await.unwrap(), 0.5);
        assert_eq!(store.get_user_spend("bob").unwrap(), 0.0);

        // User-scoped KV: only the owner's scope is reachable.
        remote.kv_put("user:alice", "k", "mine").await.unwrap();
        assert!(remote.kv_put("user:bob", "k", "theirs").await.is_err());
        assert!(remote.kv_get("user:bob", "k").await.is_err());
        assert_eq!(store.kv_get("user:alice", "k").unwrap().as_deref(), Some("mine"));
    }

    /// The transcript arms must work through the *remote* arm specifically.
    ///
    /// This is the test that would have caught the mistake worth recording
    /// here: the four `store.*` transcript methods are served by the **gateway**
    /// process, not by the worker that calls them. A worker can therefore be
    /// running a kernel that has them while the gateway is not, and the symptom
    /// is `unknown store method store.conversations` from a tree where the arm
    /// is plainly present. The local arm passing proves nothing about that path,
    /// so every method is asserted across the wire.
    #[tokio::test]
    async fn transcript_recall_works_across_the_worker_boundary() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.redb")).unwrap());

        let (gw_stream, wk_stream) = UnixStream::pair().unwrap();
        let (_gw_peer, gw_done) = ipc::Peer::spawn(gw_stream, Arc::new(GatewaySide(store.clone())));
        let (wk_peer, wk_done) = ipc::Peer::spawn(wk_stream, Arc::new(Mute));
        tokio::spawn(gw_done);
        tokio::spawn(wk_done);
        let remote = Persist::Remote(wk_peer);

        let parent = store
            .create_session(Some("the parent".into()), &"agent", "local")
            .unwrap();
        let child = store.create_session(None, &"agent", "local").unwrap();
        crate::subagents::Subagents::new(&store)
            .register(
                &parent.id,
                &child.id,
                "scout",
                "go and look",
                "",
                "",
                "plan",
                0,
            )
            .unwrap();
        store
            .append_event(
                &parent.id,
                SessionEvent::Nudge("the redb lock was the problem".into()),
            )
            .unwrap();

        // The catalogue.
        let listed = remote.conversations(false, false, 0).await.unwrap();
        assert!(listed.iter().any(|c| c.id == parent.id));
        assert!(
            !listed.iter().any(|c| c.id == child.id),
            "a sub-agent is not a conversation unless asked for"
        );

        // The sub-agent tree, for a conversation that is not the caller's own —
        // the caller here has no session at all.
        let kids = remote.conversation_subagents(&parent.id).await.unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].label, "scout");

        // A windowed read.
        let entries = remote.read_transcript(&parent.id, 0, 0, 0).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].text.contains("redb lock"));

        // And search, with the query record surviving serialisation both ways.
        let report = remote
            .search_transcripts(&crate::transcripts::SearchQuery {
                pattern: "redb lock".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(report.total_matches, 1);
        assert_eq!(report.hits[0].session_id, parent.id);

        // A bad pattern is an error on the far side, not a panic or an empty
        // result that reads as "no matches".
        assert!(
            remote
                .search_transcripts(&crate::transcripts::SearchQuery {
                    pattern: "[unclosed".into(),
                    ..Default::default()
                })
                .await
                .is_err()
        );
    }
}
