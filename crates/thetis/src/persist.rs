//! Where persistent state lives, from either side of the process split.
//!
//! redb permits one writer process, so the database belongs to the gateway
//! alone. Everything else — the worker running conversations — reaches the
//! same tables through the gateway over IPC. This enum is the seam: the same
//! call sites serve both roles, and a worker physically cannot contend for
//! the database because it never opens it.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::bindings::types::{EventRecord, SessionEvent, SessionMeta};
use crate::ipc::Peer;
use crate::store::Store;
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

    pub async fn create_session(&self, title: Option<String>, mode: &str) -> Result<SessionMeta> {
        delegate!(
            self,
            "store.create_session",
            |s| s.create_session(title.clone(), mode),
            json!({ "title": title, "mode": mode })
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
    ) -> Result<SubagentRow> {
        delegate!(
            self,
            "store.register_subagent",
            |s| crate::subagents::Subagents::new(s).register(
                parent_id,
                child_id,
                label,
                task,
                agent_aspect,
                model,
                mode,
                max_children
            ),
            json!({
                "session": parent_id,
                "child": child_id,
                "label": label,
                "task": task,
                "agent": agent_aspect,
                "model": model,
                "mode": mode,
                "max_children": max_children,
            })
        )
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
            |s| crate::subagents::Subagents::new(s).settle(
                child_id,
                result,
                cost_usd,
                stopped_by
            ),
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
            |s| crate::transcripts::Transcripts::new(s).read(
                session_id,
                from_seq,
                limit,
                max_chars
            ),
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
                peer.call_as("store.skill_vector", json!({ "key": key })).await
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
    method: &str,
    params: Value,
    caller_session: &str,
) -> Result<Value> {
    // Every arm below is a synchronous redb call, served on the gateway for a
    // worker that is waiting on a 60s RPC.
    crate::offload::blocking(|| serve_store_call_inner(store, method, params, caller_session))
}

fn serve_store_call_inner(
    store: &Store,
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
            to_value(store.create_session(title, get_str(&params, "mode")?)?)
        }
        "store.get_session" => to_value(store.get_session(get_str(&params, "id")?)?),
        "store.list_sessions" => {
            let include = params
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            to_value(store.list_sessions(include)?)
        }
        "store.rename_session" => to_value(
            store.rename_session(get_str(&params, "id")?, get_str(&params, "title")?)?,
        ),
        "store.archive_session" => {
            let archived = params
                .get("archived")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            to_value(store.archive_session(get_str(&params, "id")?, archived)?)
        }
        "store.set_mode" => to_value(
            store.set_mode(get_str(&params, "id")?, get_str(&params, "mode")?)?,
        ),
        "store.set_model" => to_value(
            store.set_model(get_str(&params, "id")?, get_str(&params, "model")?)?,
        ),
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
        "store.kv_get" => to_value(
            store.kv_get(get_str(&params, "scope")?, get_str(&params, "key")?)?,
        ),
        "store.kv_put" => to_value(store.kv_put(
            get_str(&params, "scope")?,
            get_str(&params, "key")?,
            get_str(&params, "value")?,
        )?),
        "store.kv_swap" => to_value(store.kv_swap(
            get_str(&params, "scope")?,
            get_str(&params, "key")?,
            get_str(&params, "expected")?,
            get_str(&params, "value")?,
        )?),
        "store.get_spend" => to_value(store.get_spend(own_session(store, &params, caller_session)?)?),
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
            to_value(crate::subagents::Subagents::new(store).register(
                parent,
                get_str(&params, "child")?,
                params.get("label").and_then(Value::as_str).unwrap_or(""),
                params.get("task").and_then(Value::as_str).unwrap_or(""),
                params.get("agent").and_then(Value::as_str).unwrap_or(""),
                params.get("model").and_then(Value::as_str).unwrap_or(""),
                params.get("mode").and_then(Value::as_str).unwrap_or("agent"),
                max,
            )?)
        }
        "store.get_subagent" => to_value(store.get_subagent(get_str(&params, "child")?)?),
        "store.subagents_of" => {
            to_value(store.subagents_of(own_session(store, &params, caller_session)?)?)
        }
        "store.settle_subagent" => {
            let cost = params.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
            to_value(crate::subagents::Subagents::new(store).settle(
                get_str(&params, "child")?,
                params.get("result").and_then(Value::as_str).unwrap_or(""),
                cost,
                params
                    .get("stopped_by")
                    .and_then(Value::as_str)
                    .unwrap_or("stop"),
            )?)
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
            to_value(crate::transcripts::Transcripts::new(store).conversations(
                include_archived,
                include_subagents,
                limit,
            )?)
        }
        "store.conversation_subagents" => to_value(
            crate::transcripts::Transcripts::new(store).subagents(get_str(&params, "root")?)?,
        ),
        "store.read_transcript" => {
            let from = params.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
            let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max_chars = params.get("max_chars").and_then(Value::as_u64).unwrap_or(0) as usize;
            to_value(crate::transcripts::Transcripts::new(store).read(
                get_str(&params, "id")?,
                from,
                limit,
                max_chars,
            )?)
        }
        "store.search_transcripts" => {
            let query: crate::transcripts::SearchQuery =
                serde_json::from_value(params.get("query").cloned().unwrap_or(Value::Null))?;
            to_value(crate::transcripts::Transcripts::new(store).search(&query)?)
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
                serve_store_call(&store, &method, params, "").await
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
        let meta = remote.create_session(Some("hi".into()), "build").await.unwrap();
        assert_eq!(local.get_session(&meta.id).await.unwrap().unwrap().title, "hi");
        assert_eq!(remote.get_session(&meta.id).await.unwrap().unwrap().mode, "build");

        // Events round-trip with their WIT payload intact.
        let seq = remote
            .append_event(
                &meta.id,
                SessionEvent::Nudge("steer left".into()),
            )
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

        let parent = store.create_session(Some("the parent".into()), "agent").unwrap();
        let child = store.create_session(None, "agent").unwrap();
        crate::subagents::Subagents::new(&store)
            .register(&parent.id, &child.id, "scout", "go and look", "", "", "plan", 0)
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
        assert!(remote
            .search_transcripts(&crate::transcripts::SearchQuery {
                pattern: "[unclosed".into(),
                ..Default::default()
            })
            .await
            .is_err());
    }
}
