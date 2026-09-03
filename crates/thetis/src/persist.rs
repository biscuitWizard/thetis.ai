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
    // caller's own. An empty `caller_session` means the call did not come from
    // a session-bound worker (the local test grip), so the check is skipped.
    fn own_session<'v>(params: &'v Value, caller: &str) -> Result<&'v str> {
        let session = get_str(params, "session")?;
        if !caller.is_empty() && session != caller {
            anyhow::bail!("a worker may only act on its own session");
        }
        Ok(session)
    }

    match method {
        "store.append_event" => {
            let session = own_session(&params, caller_session)?;
            let event: SessionEvent =
                serde_json::from_value(params.get("event").cloned().unwrap_or(Value::Null))?;
            to_value(store.append_event(session, event)?)
        }
        "store.events" => {
            let session = own_session(&params, caller_session)?;
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
            to_value(store.clear_resume_attempts(own_session(&params, caller_session)?)?)
        }
        "store.set_no_resume" => {
            let flag = params
                .get("no_resume")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            to_value(store.set_no_resume(own_session(&params, caller_session)?, flag)?)
        }
        "store.kv_get" => to_value(
            store.kv_get(get_str(&params, "scope")?, get_str(&params, "key")?)?,
        ),
        "store.kv_put" => to_value(store.kv_put(
            get_str(&params, "scope")?,
            get_str(&params, "key")?,
            get_str(&params, "value")?,
        )?),
        "store.get_spend" => to_value(store.get_spend(own_session(&params, caller_session)?)?),
        "store.add_spend" => {
            let usd = params.get("usd").and_then(Value::as_f64).unwrap_or(0.0);
            to_value(store.add_spend(own_session(&params, caller_session)?, usd)?)
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
}
