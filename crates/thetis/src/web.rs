//! HTTP/WebSocket transport.
//!
//! The host owns the listener and the connection registry; the gateway
//! component owns the UI and the wire protocol. The one exception is `/admin`,
//! which is rendered here in native code with no WASM in its path — it is the
//! control surface that must keep working when every guest is broken.

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

use crate::bindings::gateway::GatewayAction;
use crate::gateway;
use crate::grip::{Grip, RenderedFrame};

pub async fn serve(grip: Arc<Grip>) -> Result<()> {
    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/admin", get(admin_page))
        .route("/admin/waits", get(admin_waits))
        // One conversation's own UI build, so an agent working on the
        // interface can see its work without launching a second orchestrator.
        .route("/preview/{session}", get(preview_root))
        .route("/preview/{session}/", get(preview_root))
        .route("/preview/{session}/{*path}", get(preview_asset))
        .route("/admin/branch", post(admin_branch))
        .route("/admin/rollback", post(admin_rollback_legacy))
        // Raw workspace bytes. Separate from the frame protocol because these
        // are payloads, not messages: an image wants to be an <img> src, a
        // download wants to be a link, and an upload wants to be the browser's
        // own File stream rather than base64 inside JSON.
        .route(
            "/workspace/file/{*path}",
            get(workspace_download)
                .put(workspace_upload)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::workspace_api::MAX_UPLOAD_BYTES,
                )),
        )
        .route("/", get(root_asset))
        .route("/{*path}", get(path_asset))
        // The whole trust boundary in one layer: Thetis binds to loopback and
        // has no auth, so "a process on this machine" is the boundary. Two
        // browser-borne ways around it are closed here — a hostile page opening
        // the WebSocket (the same-origin policy does not cover WS), and a domain
        // that rebinds to 127.0.0.1 to reach the /admin forms or the upload
        // route. See `guard_local`.
        .layer(middleware::from_fn(guard_local))
        .with_state(grip.clone());

    let listener = bind_with_retry(grip.cfg.bind_addr).await?;

    tracing::info!(addr = %grip.cfg.bind_addr, "thetis listening");
    axum::serve(listener, app).await.context("http server")?;
    Ok(())
}

/// True for `127.0.0.0/8`, `localhost`, and `::1`, with or without a port.
///
/// Parses an HTTP authority (`host`, `host:port`, `[::1]`, `[::1]:port`) down
/// to its host and asks whether that host is loopback. A domain that has been
/// rebound to `127.0.0.1` still carries its own name here, so it is refused.
fn is_loopback_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: everything up to the closing bracket.
        rest.split(']').next().unwrap_or("")
    } else {
        // host:port — but a bare IPv6 has colons too; only strip a trailing
        // :port when there is exactly one colon.
        match authority.rsplit_once(':') {
            Some((h, _)) if !h.contains(':') => h,
            _ => authority,
        }
    };
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// True when an `Origin` (`scheme://authority`) is loopback. A syntactically
/// broken or opaque Origin (e.g. `null`) is not trusted.
fn is_loopback_origin(origin: &str) -> bool {
    match origin.split_once("://") {
        Some((_, authority)) => is_loopback_authority(authority),
        None => false,
    }
}

/// Refuses any request whose `Origin` or `Host` points off-loopback.
///
/// `Origin` is present on exactly the cross-context requests that matter: a
/// WebSocket handshake and a cross-site form POST both carry it, so a hostile
/// page cannot open `/ws` or forge an `/admin` submission. `Host` catches
/// DNS-rebinding, where the attacker's domain resolves to `127.0.0.1` — its
/// name still shows up here. A missing header is allowed: a non-browser client
/// on this machine (curl, the test grip) is already inside the boundary.
async fn guard_local(req: Request, next: Next) -> Response {
    let headers = req.headers();
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !is_loopback_origin(origin) {
            tracing::warn!(%origin, "refused a cross-origin request");
            return (StatusCode::FORBIDDEN, "cross-origin requests are refused").into_response();
        }
    }
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !is_loopback_authority(host) {
            tracing::warn!(%host, "refused a non-loopback Host");
            return (StatusCode::FORBIDDEN, "only loopback hosts are served").into_response();
        }
    }
    next.run(req).await
}

/// Binds, retrying briefly while the address is still in use.
///
/// A restart spawns the replacement before this process exits, so the new one
/// arrives while the old still holds the port. Waiting a few seconds is the
/// difference between a seamless restart and a failed one.
async fn bind_with_retry(addr: std::net::SocketAddr) -> Result<tokio::net::TcpListener> {
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(15);
    let deadline = std::time::Instant::now() + PATIENCE;
    let mut reported = false;

    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
                && std::time::Instant::now() < deadline =>
            {
                if !reported {
                    tracing::info!(%addr, "address busy, waiting for the previous process to exit");
                    reported = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) => return Err(anyhow::Error::from(e)).with_context(|| format!("binding {addr}")),
        }
    }
}

// Events render in the worker that produced them (with that worker's own
// build of the UI), arriving here as ready-made frames — so there is no
// render loop on this side. See `roles::worker::spawn_render_loop`.

// --- assets ----------------------------------------------------------------

async fn root_asset(State(grip): State<Arc<Grip>>) -> Response {
    asset_response(&grip, "/").await
}

async fn path_asset(State(grip): State<Arc<Grip>>, Path(path): Path<String>) -> Response {
    asset_response(&grip, &format!("/{path}")).await
}

async fn asset_response(grip: &Arc<Grip>, path: &str) -> Response {
    match gateway::serve_asset(grip, path).await {
        Ok(Some(asset)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, asset.mime)],
            asset.bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        // The gateway is broken or missing: fall back to a host-rendered page
        // so the user is never staring at a dead socket.
        Err(e) => {
            let detail = format!("{e:#}");
            tracing::warn!(error = %detail, path, "gateway asset request failed");
            (StatusCode::SERVICE_UNAVAILABLE, Html(fallback_page(&detail))).into_response()
        }
    }
}

fn fallback_page(detail: &str) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>Thetis — gateway unavailable</title>
<style>body{{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1.5rem;color:#e6e6e6;background:#16161a}}
code{{background:#26262c;padding:.15em .4em;border-radius:4px}} a{{color:#7aa2f7}}</style>
<h1>The chat gateway is unavailable</h1>
<p>The orchestrator is running, but the gateway component could not serve this page.</p>
<pre><code>{}</code></pre>
<p>The system is still recoverable: <a href="/admin">open the admin console</a> to inspect
component revisions and roll the gateway back to a working one.</p>"#,
        html_escape(detail)
    )
}

// --- workspace raw bytes ---------------------------------------------------

/// Serves one workspace file as itself.
///
/// `inline` so a browser previews rather than downloads; the explorer adds
/// `?download=1` for the save-to-disk link, which is the only difference
/// between viewing a file and keeping it.
async fn workspace_download(
    State(grip): State<Arc<Grip>>,
    Path(path): Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let resolved = match crate::workspace_api::resolve(&grip.cfg, &path) {
        Ok(resolved) => resolved,
        Err(e) => return (StatusCode::FORBIDDEN, format!("{e:#}")).into_response(),
    };
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());

    match tokio::fs::read(&resolved).await {
        Ok(bytes) => {
            // A file that a browser would execute as a document (HTML, SVG,
            // XML) must never be served inline: it would run on the gateway's
            // own origin, and from there it could open `/ws` as a genuine
            // same-origin client. Such files are forced to download; so is an
            // explicit `?download`. Everything else may preview inline.
            let active = crate::workspace_api::is_active_content(&name);
            let disposition = if active || query.contains_key("download") {
                format!("attachment; filename=\"{}\"", name.replace('"', ""))
            } else {
                "inline".to_string()
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, crate::workspace_api::mime_of(&name).to_string()),
                    (header::CONTENT_DISPOSITION, disposition),
                    // Never let the browser second-guess the declared type and
                    // sniff an upload into something executable.
                    (
                        header::X_CONTENT_TYPE_OPTIONS,
                        "nosniff".to_string(),
                    ),
                    // The agents rewrite these files under a browser that is
                    // holding one open, so a cached copy would go stale
                    // invisibly.
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, format!("cannot read {path}: {e}")).into_response(),
    }
}

/// Takes an upload straight from the browser and writes it into the workspace.
///
/// One file per request, named by the URL, so the client can report progress
/// per file and a failure names the file that failed.
async fn workspace_upload(
    State(grip): State<Arc<Grip>>,
    Path(path): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let resolved = match crate::workspace_api::resolve(&grip.cfg, &path) {
        Ok(resolved) => resolved,
        Err(e) => return (StatusCode::FORBIDDEN, format!("{e:#}")).into_response(),
    };
    if resolved.is_dir() {
        return (StatusCode::CONFLICT, format!("{path} is a directory")).into_response();
    }
    if let Some(parent) = resolved.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot create the folder: {e}"))
                .into_response();
        }
    }
    match tokio::fs::write(&resolved, &body).await {
        Ok(()) => {
            tracing::info!(path = %path, bytes = body.len(), "workspace upload");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::json!({
                    "ok": true,
                    "path": path,
                    "size": body.len(),
                })
                .to_string(),
            )
                .into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot write {path}: {e}")).into_response()
        }
    }
}

// --- admin (host-owned; never routed through a guest) -----------------------

#[derive(serde::Deserialize)]
struct BranchForm {
    action: String,
    #[serde(default)]
    target: String,
}

/// The legacy rollback endpoint: versioning moved to per-conversation
/// branches, so this only points at the new controls now.
async fn admin_rollback_legacy(State(grip): State<Arc<Grip>>) -> Html<String> {
    Html(
        render_admin(
            &grip,
            r#"<p class="banner bad">Per-aspect rollback has been replaced by per-conversation
branches. Reset a conversation's branch from within that conversation, or use the
controls below.</p>"#,
        )
        .await,
    )
}

/// The manual overrides. Deliberately plain HTML forms so they work with no
/// JavaScript, no websocket, and no guest code involved.
async fn admin_branch(
    State(grip): State<Arc<Grip>>,
    axum::Form(form): axum::Form<BranchForm>,
) -> Html<String> {
    let result = admin_branch_action(&grip, &form.action, &form.target).await;
    let banner = match result {
        Ok(message) => {
            tracing::warn!(action = %form.action, "admin: {message}");
            format!(r#"<p class="banner ok">{}</p>"#, html_escape(&message))
        }
        Err(e) => format!(
            r#"<p class="banner bad">{} failed: {}</p>"#,
            html_escape(&form.action),
            html_escape(&format!("{e:#}"))
        ),
    };
    Html(render_admin(&grip, &banner).await)
}

async fn admin_branch_action(
    grip: &Arc<Grip>,
    action: &str,
    target: &str,
) -> anyhow::Result<String> {
    let crate::grip::Role::Gateway(router) = &grip.role else {
        anyhow::bail!("admin actions run on the gateway");
    };
    let store = grip
        .local_store()
        .context("gateway has no local store")?;
    let branches = crate::branches::Branches::new(grip.cfg.clone(), store.clone());

    match action {
        // Break glass: put trunk's checkout at an earlier commit. Forward
        // history is preserved in the conversation branches that made it;
        // this moves the shared starting point everyone inherits.
        "trunk-reset" => {
            let root = branches.root_git();
            let rev = root
                .rev_parse(target)
                .await?
                .with_context(|| format!("'{target}' does not name a commit"))?;
            router.stop_all().await;
            root.hard_reset_clean(&rev).await?;
            crate::roles::gateway::load_ui_gateway(grip).await;
            Ok(format!(
                "trunk was reset to {}; stopped workers restart on their next message",
                &rev[..12]
            ))
        }
        "stop-worker" => {
            let peer = router
                .live_peer(target)
                .await
                .with_context(|| format!("no live worker for {target}"))?;
            router.mark_stopping(target).await;
            let _ = peer.call("shutdown", serde_json::Value::Null).await;
            Ok(format!("asked the worker for {target} to stop"))
        }
        "abort-merge" => {
            let state: crate::bindings::branch::BranchState = serde_json::from_value(
                crate::workers::call_session(
                    grip,
                    router,
                    target,
                    "branch.abort",
                    serde_json::json!({ "session": target }),
                )
                .await?,
            )?;
            Ok(format!(
                "merge aborted; the branch is {} again",
                state.state
            ))
        }
        "release-worktree" => {
            if router.live_peer(target).await.is_some() {
                anyhow::bail!("stop the worker first; its checkout is in use");
            }
            branches.release_worktree(target).await?;
            Ok(format!(
                "released the checkout for {target}; its branch and commits remain"
            ))
        }
        // Publishing: derive the filtered public branch, then (separately)
        // push it. Two explicit human actions, never automatic.
        "export-public" => {
            let root = branches.root_git();
            let export = crate::publish::export_public(root).await?;
            Ok(match export.public_head {
                Some(head) => format!(
                    "exported {} commit(s); public is at {}",
                    export.commits,
                    &head[..12.min(head.len())]
                ),
                None => "nothing to export yet".to_string(),
            })
        }
        "push-public" => {
            let root = branches.root_git();
            root.run_hooked(&["push", "origin", "public"], &[]).await?;
            Ok("pushed the public branch to origin".to_string())
        }
        other => anyhow::bail!("unknown action '{other}'"),
    }
}

async fn admin_page(State(grip): State<Arc<Grip>>) -> Html<String> {
    Html(render_admin(&grip, "").await)
}

async fn render_admin(grip: &Arc<Grip>, banner: &str) -> String {
    let root = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
    let trunk_name = root
        .current_branch()
        .await
        .unwrap_or_else(|_| "trunk".to_string());

    // --- trunk -------------------------------------------------------------
    let trunk_head = root.head().await.unwrap_or_default();
    let trunk_rows = root
        .log("HEAD", 15)
        .await
        .unwrap_or_default()
        .iter()
        .map(|c| {
            let is_head = c.rev == trunk_head;
            format!(
                "<tr><td class=mono>{}{}</td><td>{}</td><td class=note>{}</td><td>{}</td></tr>",
                &c.rev[..12.min(c.rev.len())],
                if is_head { " &larr; head" } else { "" },
                html_escape(&c.subject),
                html_escape(&c.author),
                if is_head {
                    String::new()
                } else {
                    format!(
                        r#"<form method=post action="/admin/branch"
                           onsubmit="return confirm('Reset trunk? All workers stop first.')">
                           <input type=hidden name=action value="trunk-reset">
                           <input type=hidden name=target value="{}">
                           <button>reset trunk here</button></form>"#,
                        c.rev
                    )
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    // --- conversations -------------------------------------------------------
    let store = grip.local_store();
    let mut branch_rows = String::new();
    if let (Some(store), crate::grip::Role::Gateway(router)) = (store, &grip.role) {
        let titles: std::collections::HashMap<String, String> = store
            .list_sessions(true)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id, s.title))
            .collect();
        let live: std::collections::HashSet<String> =
            router.live_sessions().await.into_iter().collect();

        for row in store.list_branches().unwrap_or_default() {
            let (ahead, behind) = root
                .ahead_behind(&row.branch_ref, &trunk_name)
                .await
                .unwrap_or((0, 0));
            let is_live = live.contains(&row.session_id);
            let title = titles
                .get(&row.session_id)
                .cloned()
                .unwrap_or_else(|| row.session_id.clone());

            let mut actions = String::new();
            if is_live {
                actions.push_str(&format!(
                    r#"<form method=post action="/admin/branch">
                       <input type=hidden name=action value="stop-worker">
                       <input type=hidden name=target value="{id}">
                       <button>stop worker</button></form>
                       <form method=post action="/admin/branch">
                       <input type=hidden name=action value="abort-merge">
                       <input type=hidden name=target value="{id}">
                       <button>abort merge</button></form>"#,
                    id = row.session_id
                ));
            } else {
                actions.push_str(&format!(
                    r#"<form method=post action="/admin/branch">
                       <input type=hidden name=action value="release-worktree">
                       <input type=hidden name=target value="{id}">
                       <button>release checkout</button></form>"#,
                    id = row.session_id
                ));
            }

            let kernel = if row.kernel_commit.is_empty() {
                "trunk".to_string()
            } else {
                row.kernel_commit[..12.min(row.kernel_commit.len())].to_string()
            };
            branch_rows.push_str(&format!(
                "<tr><td>{}</td><td class=mono>{}</td><td>{}</td><td class=mono>&uarr;{} &darr;{}</td>\
                 <td>{}</td><td class=mono>{}</td><td class=actions>{}</td></tr>\n",
                html_escape(&title),
                html_escape(&row.branch_ref),
                if is_live { "<span class=active>live</span>" } else { "stopped" },
                ahead,
                behind,
                format!("{:?}", row.state).to_lowercase(),
                kernel,
                actions
            ));
        }
    }

    let sessions = grip
        .persist
        .list_sessions(true)
        .await
        .map(|s| s.len())
        .unwrap_or(0);

    let private = crate::publish::private_dirs(&root, "HEAD")
        .await
        .unwrap_or_default();
    let private_list = if private.is_empty() {
        "<em>nothing</em>".to_string()
    } else {
        private
            .iter()
            .map(|p| format!("<code>{}</code>", html_escape(p)))
            .collect::<Vec<_>>()
            .join(", ")
    };

    format!(
        r#"<!doctype html><meta charset="utf-8"><title>Thetis admin</title>
<style>
 body{{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;max-width:64rem;margin:3rem auto;padding:0 1.5rem;color:#e6e6e6;background:#16161a}}
 h1{{font-size:1.4rem;margin-bottom:.2rem}} h2{{font-size:1.1rem;margin-top:2.2rem}}
 table{{border-collapse:collapse;width:100%;margin:.4rem 0 1rem}}
 th,td{{text-align:left;padding:.45rem .7rem;border-bottom:1px solid #2a2a32;vertical-align:middle}}
 th{{font-size:.7rem;text-transform:uppercase;letter-spacing:.06em;color:#9a9aa8}}
 code,.mono{{font-family:ui-monospace,monospace;background:#26262c;padding:.15em .4em;border-radius:4px}}
 button{{font:inherit;font-size:.8rem;color:#e6e6e6;background:#26262c;border:1px solid #35353f;
        border-radius:6px;padding:3px 10px;cursor:pointer}}
 button:hover{{background:#31313c}} form{{margin:0;display:inline-block}}
 .actions form{{margin-right:6px}}
 a{{color:#7aa2f7}} .note{{color:#9a9aa8;font-size:.85rem}}
 .active{{color:#9ece6a}}
 .banner{{padding:.7rem 1rem;border-radius:8px;margin:1rem 0}}
 .banner.ok{{background:#1d2a1d;border:1px solid #2f4a2f}}
 .banner.bad{{background:#2f1d21;border:1px solid #5a2f38}}
</style>
<h1>Thetis admin</h1>
<p class=note>Served directly by the orchestrator — no WebAssembly in this page's path.
It keeps working when every guest and every worker is broken.</p>
{banner}
<h2>Trunk (<code>{trunk_name}</code>)</h2>
<p class=note>What every new conversation starts from, and what everyone's page is served
from. Trunk only ever advances by merging a conversation's branch; resetting it here is
the break-glass path and stops every worker first.</p>
<table><tr><th>commit</th><th>subject</th><th>author</th><th></th></tr>
{trunk_rows}</table>
<h2>Conversations</h2>
<p class=note>Each conversation runs on its own branch in its own worker process.
&uarr; commits it has that trunk lacks; &darr; commits trunk has that it lacks.
Stopping a worker loses nothing — branch state is on disk and in the log.</p>
<table><tr><th>conversation</th><th>branch</th><th>worker</th><th>&uarr;/&darr;</th><th>state</th><th>kernel</th><th></th></tr>
{branch_rows}</table>
<h2>Publishing</h2>
<p class=note>Directories holding a <code>.thetis-private</code> marker never leave this
machine: a filtered <code>public</code> branch mirrors trunk without them, and a pre-push
hook refuses everything else. Currently private: {private_list}.</p>
<form method=post action="/admin/branch">
  <input type=hidden name=action value="export-public">
  <button>export public branch</button></form>
<form method=post action="/admin/branch"
      onsubmit="return confirm('Push the public branch to origin?')">
  <input type=hidden name=action value="push-public">
  <button>push public to origin</button></form>
<p class=note>{sessions} session(s) on record.</p>
<p><a href="/">&larr; back to chat</a></p>"#,
        banner = banner,
        trunk_name = html_escape(&trunk_name),
        trunk_rows = if trunk_rows.is_empty() {
            "<tr><td colspan=4><em>no commits</em></td></tr>".to_string()
        } else {
            trunk_rows
        },
        private_list = private_list,
        branch_rows = if branch_rows.is_empty() {
            "<tr><td colspan=7><em>no conversation branches yet</em></td></tr>".to_string()
        } else {
            branch_rows
        },
        sessions = sessions,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// --- websocket -------------------------------------------------------------

/// What the system is waiting on, as JSON.
///
/// The first thing to open when something looks frozen: it names the sessions
/// whose worker is still materializing, every outstanding RPC with its age,
/// and who holds the fleet build lock — so "the UI is stuck" becomes a page
/// load rather than an investigation with `gdb`.
async fn admin_waits(State(grip): State<Arc<Grip>>) -> Response {
    let workers = match &grip.role {
        crate::grip::Role::Gateway(router) => router.waits().await,
        crate::grip::Role::Worker(peer) => serde_json::json!({
            "pending_to_gateway": peer
                .in_flight()
                .into_iter()
                .map(|(id, method, age)| serde_json::json!({
                    "id": id, "method": method, "age_s": age
                }))
                .collect::<Vec<_>>(),
        }),
    };

    // The build lock file carries its holder's pid (written when taken), so a
    // build that is queueing the fleet can be identified from here.
    let lock = grip.cfg.build_lock_path();
    let build_lock = std::fs::read_to_string(&lock)
        .ok()
        .map(|pid| pid.trim().to_string())
        .filter(|pid| !pid.is_empty());

    let body = serde_json::json!({
        "uptime_s": crate::control::uptime().as_secs(),
        "workers": workers,
        "build_lock_holder_pid": build_lock,
        "building": grip.building_aspects(),
        "turns_running": grip.turns_in_flight(),
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )
        .into_response()
}

/// Serves `/` from a conversation's own gateway build.
async fn preview_root(
    State(grip): State<Arc<Grip>>,
    Path(session): Path<String>,
) -> Response {
    preview_response(&grip, &session, "/").await
}

async fn preview_asset(
    State(grip): State<Arc<Grip>>,
    Path((session, path)): Path<(String, String)>,
) -> Response {
    preview_response(&grip, &session, &format!("/{path}")).await
}

/// The UI a browser normally loads is trunk's, on purpose. This serves a
/// single conversation's instead, read from the shared build cache.
///
/// Assets only: the websocket, the workspace routes and everything else stay
/// on the real system, so the previewed interface drives the running Thetis
/// rather than a copy of it. That is the point — an agent wants to see its
/// interface against real conversations, which is exactly what a second
/// orchestrator on another port could not give it.
async fn preview_response(grip: &Arc<Grip>, session: &str, path: &str) -> Response {
    let loaded = match gateway::preview_component(grip, session).await {
        Ok(loaded) => loaded,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Html(fallback_page(&format!("{e:#}"))),
            )
                .into_response()
        }
    };
    match gateway::serve_preview_asset(grip, loaded, path).await {
        Ok(Some(mut asset)) => {
            if asset.mime.starts_with("text/html") {
                asset.bytes = rewrite_preview_html(&asset.bytes, session);
            }
            (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, asset.mime),
                // A preview is a moving target; a cached copy of yesterday's
                // build is the one thing it must never show.
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            asset.bytes,
        )
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            let detail = format!("{e:#}");
            tracing::warn!(error = %detail, path, session, "preview asset request failed");
            (StatusCode::SERVICE_UNAVAILABLE, Html(fallback_page(&detail))).into_response()
        }
    }
}

/// Points the previewed page's own assets at the preview, and leaves
/// everything else pointing at the real system.
///
/// The UI asks for `/app.js` absolutely, so without this a preview would serve
/// the branch's HTML and then trunk's script and stylesheet — the confusing
/// half-and-half that is worse than not working. Only references that name a
/// file are rewritten: `/admin`, `/ws` and the workspace routes are host
/// routes, and the preview is meant to drive the running system through them.
/// The scripts themselves import relatively (`./lib/dom.js`), so once the
/// entry point is under the prefix the rest follows on its own.
fn rewrite_preview_html(bytes: &[u8], session: &str) -> Vec<u8> {
    let Ok(html) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let prefix = format!("/preview/{session}");
    let mut out = String::with_capacity(html.len() + 128);
    let mut rest = html;
    loop {
        let Some((at, pat)) = ["src=\"/", "href=\"/"]
            .iter()
            .filter_map(|pat| rest.find(pat).map(|i| (i, *pat)))
            .min_by_key(|(i, _)| *i)
        else {
            break;
        };
        // Everything up to and including the opening quote.
        let attr_end = at + pat.len() - 1;
        out.push_str(&rest[..attr_end]);

        let tail = &rest[attr_end..];
        let Some(end) = tail.find('"') else {
            out.push_str(tail);
            return out.into_bytes();
        };
        let url = &tail[..end];
        // A path whose last segment has an extension is an asset this gateway
        // serves; anything else is a route on the host.
        let is_asset = url.rsplit('/').next().is_some_and(|last| last.contains('.'));
        if is_asset {
            out.push_str(&prefix);
        }
        out.push_str(url);
        out.push('"');
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out.into_bytes()
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(grip): State<Arc<Grip>>) -> Response {
    // The protocol is small JSON frames; the one bulky payload is a
    // `workspace-write` of an edited text file (bounded well under this once
    // JSON-escaped), and raw bytes go over HTTP, not here. Capping the frame
    // keeps a client from buffering an unbounded message into the gateway.
    const MAX_WS_MESSAGE: usize = 16 * 1024 * 1024;
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| connection(socket, grip))
}

/// How many outbound frames may be queued for one browser before the reader
/// starts waiting. A slow tab throttles itself; it never stalls the gateway.
const OUTBOUND_QUEUE: usize = 256;

async fn connection(socket: WebSocket, grip: Arc<Grip>) {
    let client_id = uuid::Uuid::new_v4().to_string();
    let (sink, mut incoming) = socket.split();
    let frames = grip.frames_tx.subscribe();
    // Which sessions this browser tab is currently watching. Shared, because
    // the reader subscribes and the writer filters.
    let watching: Arc<tokio::sync::RwLock<HashSet<String>>> =
        Arc::new(tokio::sync::RwLock::new(HashSet::new()));

    // The socket's two directions are two tasks. They used to be one, and a
    // single `select!` awaited every request handler inline — so a submit that
    // had to materialize a worker (up to READY_TIMEOUT) stopped this tab
    // receiving *any* frame, for every conversation it was watching, and made
    // the stop button that would have ended the wait unreachable. Nothing that
    // owns the sink may await application work.
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<String>(OUTBOUND_QUEUE);
    let writer = tokio::spawn(write_loop(
        sink,
        out_rx,
        frames,
        watching.clone(),
        client_id.clone(),
    ));

    tracing::debug!(%client_id, "websocket connected");

    while let Some(Ok(msg)) = incoming.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            // Keep-alives are handled by axum; nothing else is expected.
            _ => continue,
        };

        let frame: Option<serde_json::Value> = serde_json::from_str(&text).ok();
        let frame_type = frame
            .as_ref()
            .and_then(|f| f.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Inspection and turn control answer off to the side, concurrently.
        // These are the frames that must work *while* something else is slow —
        // the stop button most of all — and they are order-independent: they
        // read live worker state or cancel a turn, and never touch `watching`.
        if crate::debug_api::handles(&frame_type) || crate::system_api::handles(&frame_type) {
            let Some(frame) = frame else { continue };
            let grip = grip.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let replies = if crate::debug_api::handles(&frame_type) {
                    crate::debug_api::handle(&grip, &frame).await
                } else {
                    crate::system_api::handle(&grip, &frame).await
                };
                for reply in replies {
                    if out_tx.send(reply).await.is_err() {
                        return;
                    }
                }
            });
            continue;
        }

        // Everything below stays strictly ordered. A `subscribe` must land
        // before the `send` that follows it, or the tab misses its own reply.
        if let Some(frame) = &frame {
            // Branch frames are host business: git and the worker fleet live
            // here, and the guest's world is deliberately too small to reach
            // either.
            if crate::branch_api::handles(&frame_type) {
                for reply in crate::branch_api::handle(&grip, frame).await {
                    if out_tx.send(reply).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            // The shared workspace, for the same reason: the gateway guest's
            // world has no filesystem import, so the files every agent can see
            // are reachable only from here.
            if crate::workspace_api::handles(&frame_type) {
                for reply in crate::workspace_api::handle(&grip.cfg, frame).await {
                    if out_tx.send(reply).await.is_err() {
                        return;
                    }
                }
                continue;
            }
        }

        let actions = match gateway::on_client_message(&grip, &client_id, &text).await {
            Ok(actions) => actions,
            Err(e) => {
                // Show the whole chain: the outer context alone ("gateway
                // on-client-message") says nothing about what went wrong.
                let detail = format!("{e:#}");
                tracing::warn!(error = %detail, "gateway rejected a client message");
                // Naming the frame this answers lets the client tell an
                // incidental error from the refusal of the thing it is
                // waiting on — a `send` whose worker would not start arrives
                // here, and the composer stays locked behind an optimistic
                // message until it knows.
                let _ = out_tx
                    .send(error_frame(&detail, inbound_type(&text)))
                    .await;
                continue;
            }
        };

        for action in actions {
            match action {
                GatewayAction::Reply(frame) => {
                    if out_tx.send(frame).await.is_err() {
                        return;
                    }
                }
                GatewayAction::Broadcast(b) => {
                    let _ = grip.frames_tx.send(RenderedFrame {
                        session_id: b.session_id,
                        frame: b.frame,
                    });
                }
                GatewayAction::Subscribe(session_id) => {
                    watching.write().await.insert(session_id);
                }
                GatewayAction::Unsubscribe(session_id) => {
                    watching.write().await.remove(&session_id);
                }
            }
        }
    }

    drop(out_tx);
    writer.abort();
    tracing::debug!(%client_id, "websocket closed");
}

/// Owns the sink and nothing else. Moves bytes; never awaits a handler.
async fn write_loop(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut out_rx: tokio::sync::mpsc::Receiver<String>,
    mut frames: tokio::sync::broadcast::Receiver<RenderedFrame>,
    watching: Arc<tokio::sync::RwLock<HashSet<String>>>,
    client_id: String,
) {
    loop {
        tokio::select! {
            queued = out_rx.recv() => {
                let Some(text) = queued else { return };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            broadcast = frames.recv() => {
                match broadcast {
                    Ok(rendered) => {
                        if watching.read().await.contains(&rendered.session_id)
                            && sink.send(Message::Text(rendered.frame.into())).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        // Those frames are gone for good. Silently dropping
                        // them left the transcript missing messages — including
                        // the user's own — until a manual reload, so say so and
                        // let the client rebuild from the log.
                        tracing::warn!(missed, %client_id, "connection fell behind; asking it to resync");
                        let sessions: Vec<String> =
                            watching.read().await.iter().cloned().collect();
                        let resync = serde_json::json!({
                            "type": "resync",
                            "missed": missed,
                            "sessions": sessions,
                        })
                        .to_string();
                        if sink.send(Message::Text(resync.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        }
    }
}

fn error_frame(detail: &str, replying_to: Option<String>) -> String {
    serde_json::json!({
        "type": "error",
        "message": detail,
        "replying_to": replying_to,
    })
    .to_string()
}

/// The `type` of the client frame that failed, for `error.replying_to`.
fn inbound_type(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod preview_tests {
    use super::rewrite_preview_html;

    fn rewrite(html: &str) -> String {
        String::from_utf8(rewrite_preview_html(html.as_bytes(), "abc123")).unwrap()
    }

    #[test]
    fn the_pages_own_assets_are_pointed_at_the_preview() {
        // Without this the preview serves the branch's HTML and then trunk's
        // script — the half-and-half that is worse than not working at all.
        let out = rewrite(r#"<link href="/app.css"><script src="/app.js"></script>"#);
        assert!(out.contains(r#"href="/preview/abc123/app.css""#), "{out}");
        assert!(out.contains(r#"src="/preview/abc123/app.js""#), "{out}");
    }

    #[test]
    fn host_routes_are_left_alone() {
        // `/admin` and `/ws` are the real system's, and the whole point of a
        // preview is that it drives the real system.
        let out = rewrite(r#"<a href="/admin">admin</a><a href="/">home</a>"#);
        assert!(out.contains(r#"href="/admin""#), "{out}");
        assert!(!out.contains("/preview/abc123/admin"), "{out}");
    }

    #[test]
    fn inline_data_urls_and_relative_paths_are_untouched() {
        let html = r#"<link href="data:image/svg+xml,<svg/>"><script src="./lib/dom.js">"#;
        assert_eq!(rewrite(html), html);
    }

    #[test]
    fn a_page_with_nothing_to_rewrite_survives_intact() {
        let html = "<html><body>hello</body></html>";
        assert_eq!(rewrite(html), html);
    }
}

#[cfg(test)]
mod guard_tests {
    use super::{is_loopback_authority, is_loopback_origin};

    #[test]
    fn loopback_authorities_are_accepted() {
        for good in [
            "127.0.0.1",
            "127.0.0.1:7777",
            "127.0.0.1:7797",
            "localhost",
            "localhost:7777",
            "::1",
            "[::1]",
            "[::1]:7777",
        ] {
            assert!(is_loopback_authority(good), "{good} should be loopback");
        }
    }

    #[test]
    fn foreign_authorities_are_refused() {
        for bad in [
            "evil.com",
            "evil.com:7777",
            "attacker.internal",
            "10.0.0.5:7777",
            "thetis.example.com",
            // A host that merely starts with a digit but is not 127/8.
            "12.34.56.78:7777",
        ] {
            assert!(!is_loopback_authority(bad), "{bad} must be refused");
        }
    }

    #[test]
    fn origins_are_judged_by_host() {
        assert!(is_loopback_origin("http://127.0.0.1:7777"));
        assert!(is_loopback_origin("http://localhost:7777"));
        assert!(is_loopback_origin("https://[::1]:7777"));
        assert!(!is_loopback_origin("http://evil.com"));
        assert!(!is_loopback_origin("https://evil.com:7777"));
        // A hostile page's own origin, even after rebinding its DNS to
        // 127.0.0.1, still carries its name here.
        assert!(!is_loopback_origin("http://attacker.test"));
        // Opaque / malformed origins are not trusted.
        assert!(!is_loopback_origin("null"));
        assert!(!is_loopback_origin(""));
    }
}
