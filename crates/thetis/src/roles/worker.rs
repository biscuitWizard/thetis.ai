//! The worker role: one orchestrator process running conversations against
//! one source checkout.
//!
//! This is most of what the old single process was — the wasm runtime, the
//! build pipeline, the dev kit, the terminals, the watcher and watchdog —
//! minus the web server and the database, both of which belong to the
//! gateway on the other end of fd 3.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::os::fd::FromRawFd;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use crate::aspect::Aspect;
use crate::config::Config;
use crate::gateway;
use crate::grip::Grip;
use crate::ipc::{self, Handler, Peer};
use crate::pipeline;
use crate::revisions::Origin;
use crate::runtime::Runtime;
use crate::workers::WORKER_SOCKET_FD;
use crate::{watchdog, watcher};

/// The author on an inbound `submit` frame, or `None` if it names nobody.
///
/// Separate from the `submit` arm so it can be tested against the frame shapes
/// that actually arrive, which is the whole reason it is lenient. The gateway
/// runs trunk's binary while a branch worker runs its own, so a frame from an
/// older gateway simply has no `author` key — and one from a newer one could
/// have a shape this build does not know. Both must yield an unattributed
/// message, because a hard parse here would reject every message crossing that
/// gap rather than merely losing the byline on it.
fn author_on_frame(params: &Value) -> Option<crate::bindings::types::Author> {
    params
        .get("author")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        // An empty id names no principal, and would render as a blank byline.
        .filter(|a: &crate::bindings::types::Author| !a.id.is_empty())
}

/// What the worker does with requests the gateway sends it.
#[derive(Default)]
struct WorkerHandler {
    grip: OnceLock<Arc<Grip>>,
}

impl Handler for WorkerHandler {
    fn handle(
        self: Arc<Self>,
        method: String,
        params: Value,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            if method == "hello" {
                return Ok(ipc::hello_response());
            }
            let grip = self.grip.get().context("worker is still starting")?.clone();

            let session = || -> Result<String> {
                params
                    .get("session")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .context("missing 'session'")
            };

            match method.as_str() {
                "submit" => {
                    let message = params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let attachments = serde_json::from_value(
                        params.get("attachments").cloned().unwrap_or(Value::Null),
                    )
                    .unwrap_or_default();
                    // Degrades to `None` rather than failing — see
                    // `author_on_frame`, which is where the reasoning lives.
                    let author = author_on_frame(&params);
                    grip.submit(&session()?, message, attachments, author)
                        .await?;
                    Ok(Value::Null)
                }
                "cancel" => {
                    let stopped = grip.cancel(&session()?).await;
                    Ok(serde_json::json!({ "stopped": stopped }))
                }
                // Revocation: stops the turn only if the named account is its
                // speaker. The account is decided by the gateway from the
                // signed-in principal, never by anything in the worker.
                "cancel_turn_by" => {
                    let account = params
                        .get("account")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let stopped = grip.cancel_turn_by(&session()?, &account).await;
                    Ok(serde_json::json!({ "stopped": stopped }))
                }
                "resume" => {
                    grip.resume(&session()?).await;
                    Ok(Value::Null)
                }
                "agent_tools" => Ok(serde_json::to_value(grip.agent_tools(&session()?).await)?),
                // Branch operations relayed from the gateway: the same code
                // paths the agent's own branch tools use, so user- and
                // agent-initiated operations behave identically.
                "branch.status" => Ok(serde_json::to_value(
                    crate::branchops::status(&grip).await?,
                )?),
                "branch.log" => {
                    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50);
                    Ok(serde_json::to_value(
                        crate::branchops::log(&grip, limit as u32).await?,
                    )?)
                }
                "branch.update" => Ok(serde_json::to_value(
                    crate::branchops::update_from_trunk(&grip, &session()?).await?,
                )?),
                "branch.reset" => {
                    let rev = params
                        .get("rev")
                        .and_then(Value::as_str)
                        .context("missing 'rev'")?;
                    Ok(serde_json::to_value(
                        crate::branchops::reset_to(&grip, &session()?, rev).await?,
                    )?)
                }
                "branch.complete_merge" => {
                    let message = params
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    Ok(serde_json::to_value(
                        crate::branchops::complete_merge(&grip, &session()?, message).await?,
                    )?)
                }
                "branch.abort" => Ok(serde_json::to_value(
                    crate::branchops::abort_merge(&grip, &session()?).await?,
                )?),
                // A branch operation the gateway performed (a merge, a
                // handoff) — recorded here so it is rendered and persisted
                // exactly like agent-initiated ones.
                "branch.record_op" => {
                    let op: crate::bindings::types::BranchOp =
                        serde_json::from_value(params.clone()).context("unreadable branch op")?;
                    grip.append_event(
                        &session()?,
                        crate::bindings::types::SessionEvent::BranchOp(op),
                    )
                    .await?;
                    Ok(Value::Null)
                }
                "branch.commit_dirty" => {
                    crate::branchops::commit_dirty(
                        &grip,
                        params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("checkpoint"),
                    )
                    .await?;
                    Ok(Value::Null)
                }
                // What the status toolbar asks each live worker. Deliberately
                // trivial: no git, no store, no lock beyond the two counters,
                // so it answers even mid-turn — which is exactly when someone
                // is looking at it.
                "health" => Ok(serde_json::json!({
                    "turn": grip.turn_in_flight(),
                    "busy": grip.is_busy(),
                    "rss_kb": crate::system_api::self_rss_kb(),
                })),
                // Every shell this worker holds, with its transcript, for a
                // browser tab that has just opened the terminal drawer.
                "terminals.list" => Ok(serde_json::json!({
                    "terminals": grip.terminals.views().await,
                })),
                // Closing a shell from the drawer. The agent may be holding
                // this session, so the kill goes through the same `close` the
                // agent's own tool uses — process group and all — and the
                // resulting `closed` feed event is what updates every watching
                // tab, including the one that asked.
                "terminals.close" => {
                    let id = params
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() {
                        anyhow::bail!("terminals.close needs an id");
                    }
                    match grip.terminals.close(&id).await {
                        Ok(note) => Ok(serde_json::json!({
                            "ok": true,
                            "id": id,
                            "note": note,
                        })),
                        // A shell that has already gone is the outcome the
                        // caller wanted, so this is reported as success rather
                        // than as an error the drawer would have to explain.
                        Err(e) => Ok(serde_json::json!({
                            "ok": true,
                            "id": id,
                            "note": format!("{e:#}"),
                        })),
                    }
                }
                "live_revisions" => {
                    let map: std::collections::BTreeMap<String, u64> = grip
                        .loader
                        .active()
                        .into_iter()
                        .map(|(aspect, revision)| (aspect.key(), revision))
                        .collect();
                    Ok(serde_json::to_value(map)?)
                }
                "shutdown" => {
                    // The reaper asks politely (if_idle); a worker mid-turn or
                    // mid-build declines rather than dying under its work.
                    // Unconditional shutdowns (restart, archive, admin) still
                    // land immediately.
                    let if_idle = params
                        .get("if_idle")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if if_idle && grip.is_busy() {
                        return Ok(serde_json::json!({ "busy": true }));
                    }
                    // Said out loud, because `exit` is otherwise indistinguishable
                    // from being killed: a worker that vanishes silently gives
                    // whoever is debugging it nothing to go on.
                    tracing::info!(if_idle, "shutting down on request");
                    let grip = grip.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        // `exit` runs no destructors, so the shells would
                        // survive holding their pipes — and, before CLOEXEC,
                        // the gateway socket. The restart path in control.rs
                        // has always done this; the reaper path had not.
                        grip.terminals.close_all().await;
                        std::process::exit(0);
                    });
                    Ok(serde_json::json!({ "busy": false }))
                }
                other => anyhow::bail!("unknown worker method {other}"),
            }
        })
    }

    fn handle_note(self: Arc<Self>, name: String, _params: Value) {
        tracing::debug!(note = name, "ignoring unknown gateway note");
    }
}

pub async fn run(session: Option<String>, worktree: Option<std::path::PathBuf>) -> Result<()> {
    // The checkout this worker runs against. In the single-worker phase it is
    // the project root; per-conversation worktrees arrive with branching.
    if let Some(worktree) = worktree {
        std::env::set_var("THETIS_ROOT", &worktree);
    }

    // fd 3 is the gateway, inherited at spawn. Nothing else ever appears
    // there: the supervisor pins it with dup2 before exec.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(WORKER_SOCKET_FD) };
    // Close-on-exec, before anything spawns a child. `dup2` deliberately
    // clears CLOEXEC, so without this every shell the agent opens — and every
    // grandchild that outlives it — inherits the gateway socket and keeps it
    // open after this process dies. The gateway would then never see EOF and
    // the conversation would be unreachable until the gateway itself restarts.
    unsafe {
        if libc::fcntl(WORKER_SOCKET_FD, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
            return Err(std::io::Error::last_os_error())
                .context("marking the gateway socket close-on-exec");
        }
    }
    stream
        .set_nonblocking(true)
        .context("worker socket (is this process running under a gateway?)")?;
    let stream = tokio::net::UnixStream::from_std(stream)?;

    let handler = Arc::new(WorkerHandler::default());
    crate::offload::spawn_stall_detector();
    spawn_orphan_watch();
    let (peer, connection) = Peer::spawn(stream, handler.clone());
    let connection = tokio::spawn(connection);
    ipc::handshake(&peer, "worker").await?;

    let cfg = Arc::new(Config::load()?);
    tracing::info!(
        root = %cfg.root.display(),
        session = %session.as_deref().unwrap_or("-"),
        "worker starting"
    );

    let runtime = Runtime::new(cfg.clone())?;
    let grip = Grip::worker(cfg.clone(), runtime, peer.clone())?;

    // Before a single aspect is built: does this checkout's WIT agree with the
    // kernel that has to load what it produces? A branch that has not merged a
    // trunk WIT change builds guests wasmtime will refuse, and no later gate
    // can recover from it — the smoke test rejects the artifact and the green
    // fallback searches the same stale branch. Settling it here, by merging
    // trunk, is what keeps that from becoming an unloadable worker.
    let contract = pipeline::reconcile_wit_contract(&grip).await;
    contract.report();

    // Bring every aspect up. An aspect that will not start leaves the rest
    // running; the gateway's /admin stays available for a manual rollback.
    for aspect in pipeline::discover_aspects(&grip.cfg) {
        if let Err(e) = bring_up(&grip, &aspect).await {
            match contract.is_sound() {
                true => tracing::error!(%aspect, error = %e, "aspect failed to start"),
                // Not a fault in this aspect's source: nothing built in this
                // checkout can load until the contract is settled.
                false => tracing::error!(
                    %aspect, error = %e,
                    "aspect failed to start, and this checkout's WIT contract does not match \
                     the kernel — that is the cause, not this aspect's source"
                ),
            }
        }
    }

    // A stalled provider is otherwise completely silent. See
    // `spawn_retry_notices`.
    spawn_retry_notices(grip.clone());

    // This worker renders its own sessions' events and ships the frames up.
    spawn_render_loop(grip.clone(), peer.clone());
    // Shell activity goes up the same socket, so the browser can draw a live
    // terminal. The session it belongs to is not sent: the gateway end of this
    // socket already knows which conversation this worker serves, and having
    // one side of a per-conversation link restate that invites the two to
    // disagree.
    spawn_terminal_feed(grip.clone(), peer.clone());

    // The agent creates tools here. Make sure it exists before the watcher
    // starts, or newly scaffolded tools would not be watched until a restart.
    if let Err(e) = std::fs::create_dir_all(&cfg.paths.tools) {
        tracing::warn!(error = %e, "could not create the tools directory");
    }

    // Held for the process lifetime: dropping this stops hot reload.
    let _watch = match watcher::spawn(grip.clone()) {
        Ok(handle) => {
            tracing::info!("hot reload active");
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %e, "hot reload unavailable");
            None
        }
    };
    watchdog::spawn_prober(grip.clone());

    // Only now does the gateway's traffic get a grip to land on: a submit
    // or resume arriving before the aspects were up would run a turn against an
    // empty loader and fail it for nothing.
    let _ = handler.grip.set(grip.clone());
    peer.notify("ready", Value::Null).await;
    tracing::info!("worker ready");

    // Run until the gateway hangs up; without it there is nowhere to persist
    // anything, so exiting and being respawned is the correct reaction.
    let _ = connection.await;
    tracing::info!("gateway hung up; worker exiting");
    Ok(())
}

/// Writes the LLM client's retry notices into the conversation they belong to.
///
/// Without this a provider that accepts a request and never answers is
/// invisible: the read timeout is a silence, the retry is a silence, and the
/// default four attempts at 180s each is twelve minutes in which a turn is
/// indistinguishable from a hung one. The only thing that ever reached the
/// person watching was the transport error at the very end.
///
/// An incident rather than a system note, because that is what the browser
/// already renders as something gone wrong, and because it leaves the retries
/// in the log afterwards — "this turn spent eleven of its twelve minutes
/// waiting on a provider" is not reconstructible from anything else.
fn spawn_retry_notices(grip: Arc<Grip>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    grip.llm.on_retry(tx);
    tokio::spawn(async move {
        while let Some(notice) = rx.recv().await {
            if notice.session.is_empty() {
                continue;
            }
            // The budget, not just this attempt. Four attempts at a 180s read
            // timeout is twelve minutes before the turn fails, and knowing
            // that at the first notice rather than the last is the difference
            // between waiting and wondering.
            let left = notice.attempts.saturating_sub(notice.attempt);
            let text = format!(
                "Attempt {} of {} got no answer from the model provider after {}s ({}). \
                 Retrying — {} left, so up to about {} more minute(s) before this turn gives up.",
                notice.attempt,
                notice.attempts,
                notice.elapsed.as_secs(),
                notice.error,
                left,
                (u64::from(left) * notice.elapsed.as_secs()).div_ceil(60).max(1),
            );
            if let Err(e) = grip
                .persist
                .append_event(
                    &notice.session,
                    crate::bindings::types::SessionEvent::Incident(text),
                )
                .await
            {
                tracing::debug!(error = %e, "a retry notice was not recorded");
            }
        }
    });
}

/// Exits if the gateway goes away.
///
/// A worker exists to serve one conversation on behalf of one gateway; with
/// that gateway gone there is nowhere to persist anything, and a worker left
/// behind would hold a worktree and its shells indefinitely. The kernel can
/// signal this for us — `PR_SET_PDEATHSIG` — but only on the death of the
/// *thread* that forked, which for a tokio parent is an arbitrary thread that
/// may retire while the process is perfectly healthy. That killed
/// conversations mid-turn. Watching `getppid` instead asks the question we
/// actually mean, and a reparented process is unambiguous: init adopted us
/// because the gateway is gone.
fn spawn_orphan_watch() {
    let parent = unsafe { libc::getppid() };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let now = unsafe { libc::getppid() };
            if now != parent {
                tracing::warn!(
                    was = parent,
                    now,
                    "the gateway is gone; exiting rather than squatting on this worktree"
                );
                std::process::exit(0);
            }
        }
    });
}

/// Stamps parentage onto an already-rendered frame.
///
/// Done here rather than in the gateway guest deliberately. The guest renders
/// one `session-event` at a time and has no way to ask whose child a session is
/// — the contract does not tell it, and widening `outbound-event` to carry
/// parentage would change a record every guest is matched against at
/// instantiation. Adding two fields to the JSON afterwards is additive on the
/// wire, so an older UI ignores them and a newer one nests.
///
/// `agent` is the routing key the frame *came* from, so the UI can group
/// several children's interleaved frames correctly even though they all arrive
/// addressed to the parent.
fn tag_frame(frame: String, tag: &crate::delegation::ChildTag) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(&frame) else {
        return frame;
    };
    let Some(obj) = value.as_object_mut() else {
        return frame;
    };
    obj.insert("agent".into(), json!(tag.child_id));
    obj.insert("agent_label".into(), json!(tag.label));
    obj.insert("agent_parent".into(), json!(tag.parent_id));
    // The outer worker notification is routed to the root conversation, and the
    // browser also filters on the frame's own `session`. Keep those two routing
    // keys aligned: leaving the child's id here made every live child frame get
    // delivered and then discarded by the UI, while refresh appeared to repair
    // it because history already renders child events with the root id.
    obj.insert("session".into(), json!(tag.root_id));
    serde_json::to_string(&value).unwrap_or(frame)
}

fn spawn_render_loop(grip: Arc<Grip>, peer: Arc<Peer>) {
    tokio::spawn(async move {
        let mut events = grip.events_tx.subscribe();
        let mut renderer = gateway::Renderer::new(grip.clone());
        loop {
            match events.recv().await {
                Ok(event) => {
                    // Raw event first: connectors on the gateway (Discord)
                    // consume the event stream itself, not rendered frames.
                    if let Ok(raw) = serde_json::to_value(&event) {
                        peer.notify("event", raw).await;
                    }
                    let session_id = event.session_id.clone();
                    // A sub-agent's events are rendered exactly like anyone
                    // else's — it is a session and the gateway guest need not
                    // know it is a child — and then re-addressed to the
                    // conversation the user is actually watching, carrying the
                    // parentage the UI nests them under. Delivering them to the
                    // child's own id instead would put every sub-agent's work
                    // somewhere nobody has open.
                    let tag = crate::delegation::frame_tag(&grip, &session_id).await;
                    if let Some(frame) = renderer.render(event).await {
                        let (route, frame) = match &tag {
                            Some(tag) => (tag.root_id.clone(), tag_frame(frame, tag)),
                            None => (session_id, frame),
                        };
                        peer.notify("frame", json!({ "session": route, "frame": frame }))
                            .await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "renderer fell behind; some frames were dropped");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// Mirrors shell activity up to the gateway.
///
/// Coalesced, not per line: a `cargo build` writes hundreds of lines a second,
/// and one IPC note each would swamp the socket the same build's tool calls are
/// travelling on. Output for one shell is gathered for a few tens of
/// milliseconds and sent as one note; structural events (opened, command, exit,
/// closed) go straight through, because their ordering against the output is
/// what makes the transcript readable.
fn spawn_terminal_feed(grip: Arc<Grip>, peer: Arc<Peer>) {
    const FLUSH_MS: u64 = 60;
    tokio::spawn(async move {
        let mut feed = grip.terminals.subscribe();
        // id -> pending output, in arrival order.
        let mut pending: Vec<(String, String)> = Vec::new();
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(FLUSH_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event = feed.recv() => match event {
                    Ok(item) if item.kind == "output" => {
                        match pending.iter_mut().find(|(id, _)| *id == item.id) {
                            Some((_, text)) => text.push_str(&item.text),
                            None => pending.push((item.id, item.text)),
                        }
                    }
                    Ok(item) => {
                        // Flush first, or a command's output would arrive after
                        // the "exit" that ended it.
                        flush(&peer, &mut pending).await;
                        peer.notify("terminal", json!({
                            "id": item.id,
                            "kind": item.kind,
                            "text": item.text,
                            "cwd": item.cwd,
                            "shell": item.shell,
                            "remote": item.remote,
                        }))
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(missed, "terminal feed fell behind");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                _ = ticker.tick() => flush(&peer, &mut pending).await,
            }
        }
    });
}

async fn flush(peer: &Arc<Peer>, pending: &mut Vec<(String, String)>) {
    for (id, text) in pending.drain(..) {
        peer.notify(
            "terminal",
            json!({ "id": id, "kind": "output", "text": text }),
        )
        .await;
    }
}

async fn bring_up(grip: &Arc<Grip>, aspect: &Aspect) -> Result<()> {
    // A clean checkout whose tree has a cached, smoke-passing artifact loads
    // instantly and needs no toolchain — the common case for every branch
    // materialized at trunk, since trunk's builds are already in the cache.
    let dirty = match &grip.git {
        Some(git) => git.is_dirty().await.unwrap_or(true),
        None => true,
    };
    if !dirty {
        if let Some(key) = pipeline::aspect_cache_key(grip, "HEAD", aspect).await {
            if let Some(meta) = grip.buildcache.lookup(&aspect.key(), &key)? {
                let artifact = grip
                    .buildcache
                    .artifact_path(&meta, pipeline::CACHE_ARTIFACT)?;
                match crate::loader::Loader::compile(
                    &grip.runtime.engine,
                    aspect,
                    pipeline::key_revision(&key),
                    &artifact,
                ) {
                    Ok(component) => {
                        grip.install_component(component).await;
                        tracing::info!(%aspect, "loaded from the build cache");
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!(%aspect, error = %e, "cached artifact would not load; rebuilding");
                    }
                }
            }
        }
    }

    tracing::info!(%aspect, "building");
    let outcome = pipeline::build_and_activate(grip, aspect, Origin::Bootstrap, "startup").await?;

    if !outcome.success {
        // Last resort: put the tree back at this branch's last green build.
        // A successful reset leaves the aspect serving that build, so this is
        // degradation, not failure — the agent can read the compile error in
        // its own tree and carry on.
        match pipeline::reset_aspect_to_green(grip, aspect).await {
            Ok(message) => {
                tracing::warn!(
                    %aspect,
                    "{message} — the current source does not build: {}",
                    outcome.detail
                );
                return Ok(());
            }
            Err(reset_err) => {
                anyhow::bail!(
                    "{}\n{}\n(and no green build to fall back to: {reset_err:#})",
                    outcome.detail,
                    outcome.stderr
                );
            }
        }
    }

    tracing::info!(
        %aspect,
        revision = outcome.revision.unwrap_or(0),
        took_ms = outcome.duration_ms,
        "loaded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_author_is_read_off_a_submit_frame() {
        let a = author_on_frame(&json!({
            "author": { "id": "alice", "display": "Alice", "surface": "web" }
        }))
        .expect("a well-formed author is taken");
        assert_eq!(a.id, "alice");
        assert_eq!(a.display, "Alice");
    }

    #[test]
    fn a_frame_from_an_older_gateway_is_unattributed_not_rejected() {
        // The gateway runs trunk's binary while a branch worker runs its own,
        // so this is the shape that arrives across that gap for real. Failing
        // here would reject the *message*, not just its byline.
        assert!(author_on_frame(&json!({ "session": "s", "message": "hi" })).is_none());
        assert!(author_on_frame(&json!({ "author": null })).is_none());
    }

    #[test]
    fn an_unrecognisable_author_is_dropped_rather_than_trusted() {
        // A shape this build does not understand, and an author naming nobody.
        // Both must read as absent: an empty id resolves to no principal, and
        // letting it through would put a blank byline in the transcript and an
        // empty speaker into policy resolution.
        assert!(author_on_frame(&json!({ "author": "alice" })).is_none());
        assert!(author_on_frame(&json!({ "author": { "id": "alice" } })).is_none());
        assert!(
            author_on_frame(&json!({
                "author": { "id": "", "display": "Nobody", "surface": "web" }
            }))
            .is_none()
        );
    }

    #[test]
    fn child_frame_is_addressed_to_the_root_conversation() {
        let tag = crate::delegation::ChildTag {
            child_id: "child".into(),
            parent_id: "parent".into(),
            root_id: "root".into(),
            label: "research".into(),
        };
        let tagged = tag_frame(
            serde_json::json!({"type": "event", "session": "child", "kind": "turn-started"})
                .to_string(),
            &tag,
        );
        let value: Value = serde_json::from_str(&tagged).unwrap();
        assert_eq!(value["session"], "root");
        assert_eq!(value["agent"], "child");
        assert_eq!(value["agent_parent"], "parent");
        assert_eq!(value["agent_label"], "research");
    }
}
