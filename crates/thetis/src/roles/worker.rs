//! The worker role: one orchestrator process running conversations against
//! one source checkout.
//!
//! This is most of what the old single process was — the wasm runtime, the
//! build pipeline, the dev kit, the terminals, the watcher and watchdog —
//! minus the web server and the database, both of which belong to the
//! gateway on the other end of fd 3.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::os::fd::FromRawFd;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use crate::config::Config;
use crate::gateway;
use crate::grip::Grip;
use crate::ipc::{self, Handler, Peer};
use crate::pipeline;
use crate::revisions::Origin;
use crate::runtime::Runtime;
use crate::aspect::Aspect;
use crate::workers::WORKER_SOCKET_FD;
use crate::{watchdog, watcher};

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
            let grip = self
                .grip
                .get()
                .context("worker is still starting")?
                .clone();

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
                    grip.submit(&session()?, message, attachments).await?;
                    Ok(Value::Null)
                }
                "cancel" => {
                    let stopped = grip.cancel(&session()?).await;
                    Ok(serde_json::json!({ "stopped": stopped }))
                }
                "resume" => {
                    grip.resume(&session()?).await;
                    Ok(Value::Null)
                }
                "agent_tools" => Ok(serde_json::to_value(
                    grip.agent_tools(&session()?).await,
                )?),
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
                        serde_json::from_value(params.clone())
                            .context("unreadable branch op")?;
                    grip
                        .append_event(
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

pub async fn run(
    session: Option<String>,
    worktree: Option<std::path::PathBuf>,
) -> Result<()> {
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
            return Err(std::io::Error::last_os_error()).context("marking the gateway socket close-on-exec");
        }
    }
    stream
        .set_nonblocking(true)
        .context("worker socket (is this process running under a gateway?)")?;
    let stream = tokio::net::UnixStream::from_std(stream)?;

    let handler = Arc::new(WorkerHandler::default());
    crate::offload::spawn_stall_detector();
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

    // Bring every aspect up. An aspect that will not start leaves the rest
    // running; the gateway's /admin stays available for a manual rollback.
    for aspect in pipeline::discover_aspects(&grip.cfg) {
        if let Err(e) = bring_up(&grip, &aspect).await {
            tracing::error!(%aspect, error = %e, "aspect failed to start");
        }
    }

    // This worker renders its own sessions' events and ships the frames up.
    spawn_render_loop(grip.clone(), peer.clone());

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
                    if let Some(frame) = renderer.render(event).await {
                        peer.notify("frame", json!({ "session": session_id, "frame": frame }))
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
