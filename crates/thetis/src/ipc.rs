//! Gateway ↔ worker RPC.
//!
//! One conversation runs in one worker process; the gateway owns the database
//! and the browsers. Everything between them travels over a Unix socketpair
//! the worker inherits at spawn — no ports, no socket files, and a dead peer
//! is an EOF rather than a timeout.
//!
//! The protocol is deliberately boring: one JSON object per line, either a
//! request (`{id, method, params}`), a response (`{id, result}` or
//! `{id, error}`), or a one-way note (`{note, params}`). Both ends can send
//! requests — the gateway drives the worker (submit, cancel, shutdown) and
//! the worker leans on the gateway for everything persistent (events, kv,
//! spend). Unknown fields are ignored everywhere, because a branch worker may
//! be running a *modified* kernel that has learned new tricks.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};

/// Bumped when the protocol changes shape incompatibly. The handshake rejects
/// a mismatch, and the supervisor falls back to the trunk kernel — a branch
/// that rewrote its IPC must first merge a gateway that understands it.
pub const PROTOCOL_VERSION: u64 = 1;

/// How long a call may wait for its answer. Calls are either quick lookups or
/// commands that themselves return quickly (a submit *starts* a turn, it does
/// not wait for one), so anything slower means the peer is wedged.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The budget for a method that is *known* to take real time — the branch
/// operations, which shell out to git.
///
/// `GIT_TIMEOUT` is 120s per git command and a branch operation runs a dozen
/// of them in sequence, so the default 60s was shorter than a single one of
/// its steps. The caller gave up while the work carried on: the UI reported a
/// failure for a merge that had in fact landed, or — worse — for one that was
/// half-applied, with trunk already merged into the branch. Waiting is now
/// cheap, because the websocket no longer blocks on it.
pub const SLOW_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long one frame may take to reach the peer. Shorter than `CALL_TIMEOUT`
/// on purpose: a caller that cannot even *send* should not consume the whole
/// answer budget, and every other caller is queued behind it meanwhile.
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Serialize, Deserialize)]
struct WireRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireResponse {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireNote {
    note: String,
    #[serde(default)]
    params: Value,
}

/// What one endpoint does with traffic the other end initiates.
pub trait Handler: Send + Sync + 'static {
    /// Answer a request. The returned value travels back as the result.
    fn handle(
        self: Arc<Self>,
        method: String,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>>;

    /// Absorb a one-way note. Must not block.
    fn handle_note(self: Arc<Self>, name: String, params: Value);
}

/// One end of a gateway↔worker connection.
pub struct Peer {
    writer: Mutex<OwnedWriteHalf>,
    pending: StdMutex<HashMap<u64, PendingCall>>,
    next_id: AtomicU64,
    /// Set once the read loop exits; every call fails fast from then on.
    closed: std::sync::atomic::AtomicBool,
}

impl Peer {
    /// Wires a stream to a handler and starts the read loop. The returned
    /// future resolves when the peer hangs up — the caller decides whether
    /// that is a shutdown or a death.
    pub fn spawn(
        stream: UnixStream,
        handler: Arc<dyn Handler>,
    ) -> (Arc<Peer>, impl Future<Output = ()> + Send) {
        let (read, write) = stream.into_split();
        let peer = Arc::new(Peer {
            writer: Mutex::new(write),
            pending: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: std::sync::atomic::AtomicBool::new(false),
        });

        let loop_peer = peer.clone();
        let done = async move {
            read_loop(loop_peer.clone(), read, handler).await;
            loop_peer.closed.store(true, Ordering::SeqCst);
            // Wake every caller still waiting; their sends fail as the map drops.
            if let Ok(mut pending) = loop_peer.pending.lock() {
                pending.clear();
            }
        };
        (peer, done)
    }

    /// Sends a request and waits for its response, up to [`CALL_TIMEOUT`].
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_within(method, params, CALL_TIMEOUT).await
    }

    /// As [`call`](Self::call), with an explicit deadline.
    ///
    /// A method whose own work is slower than the default budget must say so
    /// here rather than being abandoned mid-flight — an abandoned call does
    /// not cancel anything, it just stops anyone hearing the answer.
    pub async fn call_within(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("ipc peer is gone (calling {method})");
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("ipc pending map poisoned"))?
            .insert(
                id,
                PendingCall {
                    tx,
                    method: method.to_string(),
                    since: std::time::Instant::now(),
                },
            );
        // The entry is removed however this call ends — including by the
        // caller's own future being dropped, which no explicit `remove` can
        // catch. `system_api`'s health poll cancels a `peer.call` every 1.5s,
        // so against a wedged worker that leaked one entry per poll, forever.
        let _pending = PendingSlot { peer: self, id };

        // The peer may have died between the check above and the insert, in
        // which case the read loop has already drained the map and nothing
        // will ever wake this caller.
        if self.closed.load(Ordering::SeqCst) {
            bail!("ipc peer is gone (calling {method})");
        }

        let line = serde_json::to_string(&WireRequest {
            id,
            method: method.to_string(),
            params,
        })?;
        self.send_line(&line).await?;

        let response = match tokio::time::timeout(limit, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => bail!("ipc peer hung up while {method} was in flight"),
            Err(_) => {
                bail!("ipc call {method} timed out after {}s", limit.as_secs());
            }
        };

        match response.error {
            Some(message) => Err(anyhow!("{message}")),
            None => Ok(response.result.unwrap_or(Value::Null)),
        }
    }

    /// Typed convenience over `call`.
    pub async fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value)
            .with_context(|| format!("decoding the response to {method}"))
    }

    /// Sends a one-way note. Failures are logged, not returned: notes carry
    /// best-effort traffic (frames, status) whose loss the system survives.
    pub async fn notify(&self, name: &str, params: Value) {
        let Ok(line) = serde_json::to_string(&WireNote {
            note: name.to_string(),
            params,
        }) else {
            return;
        };
        if let Err(e) = self.send_line(&line).await {
            tracing::debug!(note = name, error = %e, "ipc note not delivered");
        }
    }

    /// The request ids still waiting for an answer, oldest first.
    ///
    /// Purely diagnostic: when a conversation looks frozen, this is what says
    /// whether it is waiting on the peer at all.
    pub fn in_flight(&self) -> Vec<(u64, String, u64)> {
        let Ok(pending) = self.pending.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<(u64, String, u64)> = pending
            .iter()
            .map(|(id, aspect)| (*id, aspect.method.clone(), aspect.since.elapsed().as_secs()))
            .collect();
        rows.sort_by_key(|(id, _, _)| *id);
        rows
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Declares the peer dead without waiting for its socket to close.
    ///
    /// EOF is the usual death signal, but a worker's grandchild can inherit
    /// the socket and hold it open long after the worker itself is gone. The
    /// supervisor, which watches the *process*, calls this so every in-flight
    /// caller fails at once instead of waiting out `CALL_TIMEOUT` against a
    /// corpse.
    pub fn force_close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }

    async fn send_line(&self, line: &str) -> Result<()> {
        let mut writer = self.writer.lock().await;
        // Bounded, because this holds the peer's single writer lock. A peer
        // that has stopped reading fills the socket buffer, the write parks,
        // and with no deadline *every other* caller queues behind this one —
        // one stuck frame freezing the whole conversation in both directions.
        // Failing here is honest: the peer is not reading.
        let write = async {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        };
        match tokio::time::timeout(SEND_TIMEOUT, write).await {
            Ok(result) => result?,
            Err(_) => {
                // Half a frame may have gone out, so the stream is no longer
                // parseable by the peer. Treat the connection as lost.
                self.force_close();
                bail!(
                    "ipc peer stopped reading (a frame stalled for {}s)",
                    SEND_TIMEOUT.as_secs()
                );
            }
        }
        Ok(())
    }
}

/// Removes a request from the pending map when the call leaves scope, by any
/// route: an answer, a timeout, an error, or the caller being cancelled.
struct PendingSlot<'a> {
    peer: &'a Peer,
    id: u64,
}

impl Drop for PendingSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.peer.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// One request awaiting its answer. The method and age are carried purely so
/// a wedged peer can say what it is stuck on.
struct PendingCall {
    tx: oneshot::Sender<WireResponse>,
    method: String,
    since: std::time::Instant,
}

async fn read_loop(
    peer: Arc<Peer>,
    read: tokio::net::unix::OwnedReadHalf,
    handler: Arc<dyn Handler>,
) {
    let mut lines = BufReader::new(read).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return, // clean EOF: the peer exited
            Err(e) => {
                tracing::warn!(error = %e, "ipc read failed");
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        // Shape sniffing: a request has "method", a note has "note",
        // everything else with an id is a response.
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(error = %e, "ipc line was not JSON; ignoring");
                continue;
            }
        };

        if value.get("method").is_some() {
            let Ok(request) = serde_json::from_value::<WireRequest>(value) else {
                continue;
            };
            let peer = peer.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let outcome = handler.handle(request.method.clone(), request.params).await;
                let response = match outcome {
                    Ok(result) => WireResponse {
                        id: request.id,
                        result: Some(result),
                        error: None,
                    },
                    Err(e) => WireResponse {
                        id: request.id,
                        result: None,
                        error: Some(format!("{e:#}")),
                    },
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = peer.send_line(&line).await;
                }
            });
        } else if value.get("note").is_some() {
            if let Ok(note) = serde_json::from_value::<WireNote>(value) {
                handler.clone().handle_note(note.note, note.params);
            }
        } else if let Ok(response) = serde_json::from_value::<WireResponse>(value) {
            let waiter = peer
                .pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&response.id));
            if let Some(waiter) = waiter {
                let _ = waiter.tx.send(response);
            }
        }
    }
}

/// The version handshake, run by both ends before anything else. Symmetric:
/// each side announces, each side checks.
pub async fn handshake(peer: &Peer, role: &str) -> Result<()> {
    let theirs: Value = peer
        .call(
            "hello",
            serde_json::json!({ "version": PROTOCOL_VERSION, "role": role }),
        )
        .await
        .context("ipc handshake")?;
    let version = theirs.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != PROTOCOL_VERSION {
        bail!("ipc protocol mismatch: ours {PROTOCOL_VERSION}, theirs {version}");
    }
    Ok(())
}

/// Answers the `hello` request on behalf of any handler.
pub fn hello_response() -> Value {
    serde_json::json!({ "version": PROTOCOL_VERSION })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Handler for Echo {
        fn handle(
            self: Arc<Self>,
            method: String,
            params: Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
            Box::pin(async move {
                match method.as_str() {
                    "hello" => Ok(hello_response()),
                    "echo" => Ok(params),
                    "boom" => Err(anyhow!("deliberate failure")),
                    "slow" => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(Value::String("done".into()))
                    }
                    other => Err(anyhow!("unknown method {other}")),
                }
            })
        }

        fn handle_note(self: Arc<Self>, _name: String, _params: Value) {}
    }

    fn pair() -> (Arc<Peer>, Arc<Peer>) {
        let (a, b) = UnixStream::pair().unwrap();
        let (peer_a, done_a) = Peer::spawn(a, Arc::new(Echo));
        let (peer_b, done_b) = Peer::spawn(b, Arc::new(Echo));
        tokio::spawn(done_a);
        tokio::spawn(done_b);
        (peer_a, peer_b)
    }

    #[tokio::test]
    async fn round_trip_and_errors() {
        let (a, _b) = pair();
        let out = a
            .call("echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({"x": 1}));

        let err = a.call("boom", Value::Null).await.unwrap_err();
        assert!(err.to_string().contains("deliberate failure"));
    }

    #[tokio::test]
    async fn concurrent_calls_correlate_correctly() {
        let (a, _b) = pair();
        let mut handles = Vec::new();
        for i in 0..32 {
            let a = a.clone();
            handles.push(tokio::spawn(async move {
                let params = serde_json::json!({ "n": i });
                let out = a.call("echo", params.clone()).await.unwrap();
                assert_eq!(out, params);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn both_directions_work() {
        let (a, b) = pair();
        let from_a = a.call("echo", serde_json::json!("ping")).await.unwrap();
        let from_b = b.call("echo", serde_json::json!("pong")).await.unwrap();
        assert_eq!(from_a, serde_json::json!("ping"));
        assert_eq!(from_b, serde_json::json!("pong"));
    }

    #[tokio::test]
    async fn handshake_agrees_on_version() {
        let (a, _b) = pair();
        handshake(&a, "test").await.unwrap();
    }

    #[tokio::test]
    async fn peer_death_fails_in_flight_and_future_calls() {
        // The far end is a raw stream that never answers — dropping it is
        // what a worker crash looks like from the gateway.
        let (a_stream, b_stream) = UnixStream::pair().unwrap();
        let (a, done_a) = Peer::spawn(a_stream, Arc::new(Echo));
        tokio::spawn(done_a);

        let in_flight = {
            let a = a.clone();
            tokio::spawn(async move { a.call("slow", Value::Null).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(b_stream);

        let outcome = in_flight.await.unwrap();
        assert!(outcome.is_err(), "in-flight call must fail, not hang");

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(a.is_closed());
        assert!(a.call("echo", Value::Null).await.is_err());
    }

    #[tokio::test]
    async fn notes_are_fire_and_forget() {
        struct Collect(StdMutex<Vec<String>>);
        impl Handler for Collect {
            fn handle(
                self: Arc<Self>,
                _method: String,
                _params: Value,
            ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
                Box::pin(async { Ok(Value::Null) })
            }
            fn handle_note(self: Arc<Self>, name: String, _params: Value) {
                self.0.lock().unwrap().push(name);
            }
        }

        let (x, y) = UnixStream::pair().unwrap();
        let collector = Arc::new(Collect(StdMutex::new(Vec::new())));
        let (_peer_x, done_x) = Peer::spawn(x, collector.clone());
        let (peer_y, done_y) = Peer::spawn(y, Arc::new(Echo));
        tokio::spawn(done_x);
        tokio::spawn(done_y);

        peer_y.notify("frame", serde_json::json!({"s": "abc"})).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(collector.0.lock().unwrap().as_slice(), ["frame"]);
    }

    #[tokio::test]
    async fn junk_lines_are_ignored() {
        let (x, y) = UnixStream::pair().unwrap();
        let (peer_x, done_x) = Peer::spawn(x, Arc::new(Echo));
        tokio::spawn(done_x);

        // Write garbage directly, then a real request still succeeds.
        let (_read, mut write) = y.into_split();
        write.write_all(b"not json at all\n\n").await.unwrap();
        write
            .write_all(b"{\"weird\": \"shape\"}\n")
            .await
            .unwrap();
        write.flush().await.unwrap();

        // peer_x should still be alive; prove it by asking it to handle a
        // request written raw onto the socket.
        write
            .write_all(b"{\"id\": 7, \"method\": \"echo\", \"params\": 42}\n")
            .await
            .unwrap();
        write.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!peer_x.is_closed());
    }
}
