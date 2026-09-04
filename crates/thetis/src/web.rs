//! HTTP/WebSocket transport.
//!
//! The host owns the listener and the connection registry; the gateway
//! component owns the UI and the wire protocol. The one exception is `/admin`,
//! which is rendered here in native code with no WASM in its path — it is the
//! control surface that must keep working when every guest is broken.

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Form, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
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
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/api/me", get(whoami))
        .route("/admin", get(admin_page))
        .route("/admin/waits", get(admin_waits))
        // One conversation's own UI build, so an agent working on the
        // interface can see its work without launching a second orchestrator.
        .route("/preview/{session}", get(preview_root))
        .route("/preview/{session}/", get(preview_root))
        .route("/preview/{session}/{*path}", get(preview_asset))
        .route("/admin/branch", post(admin_branch))
        .route("/admin/user/logout", post(admin_user_logout))
        .route("/admin/rollback", post(admin_rollback_legacy))
        // Raw workspace bytes. Separate from the frame protocol because these
        // are payloads, not messages: an image wants to be an <img> src, a
        // download wants to be a link, and an upload wants to be the browser's
        // own File stream rather than base64 inside JSON.
        .route(
            "/workspace/file/{*path}",
            get(workspace_download).put(workspace_upload).layer(
                axum::extract::DefaultBodyLimit::max(crate::workspace_api::MAX_UPLOAD_BYTES),
            ),
        )
        .route("/", get(root_asset))
        .route("/{*path}", get(path_asset))
        // Identity and authorization live in this native router. The outer
        // origin guard preserves same-origin/Host protection; authentication
        // then attaches a principal before any user-facing handler runs.
        .layer(middleware::from_fn_with_state(grip.clone(), authenticate))
        .layer(middleware::from_fn_with_state(grip.clone(), guard_origin))
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
#[cfg(test)]
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
#[cfg(test)]
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

/// Whether an HTTP authority may reach this server: loopback always, plus the
/// one `server.public_origin` names. Nothing else — a reverse proxy in front
/// is expected to forward `Host` unchanged, and `X-Forwarded-*` is not read.
fn authority_allowed(public_origin: Option<&crate::config::Origin>, authority: &str) -> bool {
    is_loopback_authority(authority) || public_origin.is_some_and(|o| o.authority == authority)
}

/// Whether an `Origin` header (`scheme://authority`) may reach this server.
/// An opaque or malformed origin (`null`) is not trusted.
fn origin_allowed(public_origin: Option<&crate::config::Origin>, origin: &str) -> bool {
    match origin.split_once("://") {
        Some((_, authority)) => authority_allowed(public_origin, authority),
        None => false,
    }
}

/// `guard_local` with one extra allowed authority. In local mode there is
/// none, and the behaviour is byte for byte the loopback-only rule above.
async fn guard_origin(State(grip): State<Arc<Grip>>, req: Request, next: Next) -> Response {
    let public = grip.cfg.public_origin.as_ref();
    let headers = req.headers();
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_allowed(public, origin) {
            tracing::warn!(%origin, "refused a cross-origin request");
            return (StatusCode::FORBIDDEN, "cross-origin requests are refused").into_response();
        }
    }
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !authority_allowed(public, host) {
            tracing::warn!(%host, "refused a Host that is neither loopback nor the public origin");
            return (StatusCode::FORBIDDEN, "host is not allowed").into_response();
        }
    }
    next.run(req).await
}
async fn authenticate(State(grip): State<Arc<Grip>>, mut req: Request, next: Next) -> Response {
    let public = matches!(req.uri().path(), "/login" | "/logout");
    match crate::auth::resolve(&grip, req.headers()).await {
        Some(p) => {
            if req.uri().path() == "/login" && grip.cfg.auth.users_mode {
                return Redirect::to("/").into_response();
            }
            if req.uri().path().starts_with("/admin") && (!grip.cfg.admin_enabled || !p.is_admin())
            {
                return (StatusCode::FORBIDDEN, "admin console unavailable").into_response();
            }
            req.extensions_mut().insert(p);
            next.run(req).await
        }
        None if public => next.run(req).await,
        None => {
            if req.method() == axum::http::Method::GET
                && req
                    .headers()
                    .get(header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.contains("text/html"))
            {
                let next = req
                    .uri()
                    .path_and_query()
                    .map(|v| v.as_str())
                    .unwrap_or("/");
                Redirect::to(&format!("/login?next={}", percent_encode(next))).into_response()
            } else {
                (StatusCode::UNAUTHORIZED, "sign in first").into_response()
            }
        }
    }
}
fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[derive(serde::Deserialize, Default)]
struct LoginQuery {
    #[serde(default)]
    next: String,
}
#[derive(serde::Deserialize)]
struct LoginForm {
    user: String,
    password: String,
    #[serde(default)]
    next: String,
}
async fn login_page(State(g): State<Arc<Grip>>, Query(q): Query<LoginQuery>) -> Response {
    if !g.cfg.auth.users_mode {
        return Redirect::to("/").into_response();
    }
    crate::auth::page(&g.cfg, None, &q.next).into_response()
}
async fn login_submit(
    State(g): State<Arc<Grip>>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    if !g.cfg.auth.users_mode {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Looked up the way people type it: `Alice` finds `alice`.
    let typed = f.user.trim().to_lowercase();
    // Cooling off is decided before the password is looked at, and for names
    // that do not exist too, so a guess-the-username loop gets the same
    // answer as a guess-the-password one.
    if crate::auth::login_locked(&typed, &g.cfg) {
        return crate::auth::page(
            &g.cfg,
            Some("Too many attempts. Try again in a minute."),
            &f.next,
        )
        .into_response();
    }
    let user = g.cfg.auth.user(&typed);
    // An unknown user costs the same argon2 work as a wrong password, so the
    // response time does not say which accounts exist.
    let ok = match user {
        Some(u) => crate::auth::verify_password(&f.password, u.password_hash.expose()),
        None => {
            let _ = crate::auth::verify_password(&f.password, crate::auth::dummy_hash());
            false
        }
    };
    let Some(u) = user.filter(|_| ok) else {
        crate::auth::login_failed(&typed, &g.cfg);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return crate::auth::page(&g.cfg, Some("Wrong user or password."), &f.next).into_response();
    };
    crate::auth::login_succeeded(&typed);
    let t = crate::auth::new_token();
    let now = crate::store::now_ms();
    let row = crate::store::LoginRow {
        user_id: u.id.clone(),
        created_ms: now,
        last_seen_ms: now,
        expires_ms: now + g.cfg.auth.session_ttl.as_millis() as u64,
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
    };
    match g.local_store() {
        Some(s) => {
            if let Err(e) = s.put_login(&crate::auth::token_hash(&t), &row) {
                tracing::error!(error = %e, user = %u.id, "could not record a login");
                return (StatusCode::INTERNAL_SERVER_ERROR, "could not record the login")
                    .into_response();
            }
        }
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no store").into_response(),
    }
    tracing::info!(user = %u.id, "signed in");
    (
        [(header::SET_COOKIE, crate::auth::set_cookie(&g.cfg, &t))],
        Redirect::to(&crate::auth::safe_next(&f.next)),
    )
        .into_response()
}
async fn logout(State(g): State<Arc<Grip>>, headers: HeaderMap) -> Response {
    if !g.cfg.auth.users_mode {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let (Some(t), Some(s)) = (crate::auth::cookie_value(&headers), g.local_store()) {
        let _ = s.remove_login(&crate::auth::token_hash(&t));
    }
    (
        [(header::SET_COOKIE, crate::auth::clear_cookie())],
        Redirect::to("/login"),
    )
        .into_response()
}
/// Who the caller is. The same summary the socket sends as its first frame;
/// this is what the UI asks when the socket keeps being refused, to tell an
/// expired login (401, go to the door) from a gateway that is merely down.
async fn whoami(Extension(p): Extension<Arc<crate::auth::Principal>>) -> Response {
    axum::Json(p.describe()).into_response()
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
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrInUse
                    && std::time::Instant::now() < deadline =>
            {
                if !reported {
                    tracing::info!(%addr, "address busy, waiting for the previous process to exit");
                    reported = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) => {
                return Err(anyhow::Error::from(e)).with_context(|| format!("binding {addr}"));
            }
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
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(fallback_page(&detail)),
            )
                .into_response()
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
    Extension(principal): Extension<Arc<crate::auth::Principal>>,
    Path(path): Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if principal.policy.denies(crate::policy::Cap::Workspace) {
        return (StatusCode::FORBIDDEN, "workspace access is withheld").into_response();
    }
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
                    (
                        header::CONTENT_TYPE,
                        crate::workspace_api::mime_of(&name).to_string(),
                    ),
                    (header::CONTENT_DISPOSITION, disposition),
                    // Never let the browser second-guess the declared type and
                    // sniff an upload into something executable.
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
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
    Extension(principal): Extension<Arc<crate::auth::Principal>>,
    Path(path): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if principal.policy.denies(crate::policy::Cap::WorkspaceWrite) {
        return (StatusCode::FORBIDDEN, "workspace writes are withheld").into_response();
    }
    let resolved = match crate::workspace_api::resolve(&grip.cfg, &path) {
        Ok(resolved) => resolved,
        Err(e) => return (StatusCode::FORBIDDEN, format!("{e:#}")).into_response(),
    };
    if resolved.is_dir() {
        return (StatusCode::CONFLICT, format!("{path} is a directory")).into_response();
    }
    if let Some(parent) = resolved.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot create the folder: {e}"),
            )
                .into_response();
        }
    }
    match tokio::fs::write(&resolved, &body).await {
        Ok(()) => {
            // A new file must show up in the composer's `@` menu without
            // waiting out the index's TTL.
            crate::workspace_api::invalidate_index();
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot write {path}: {e}"),
        )
            .into_response(),
    }
}

// --- admin (host-owned; never routed through a guest) -----------------------
//
// The controls themselves live in `admin.rs`, shared with the `admin` import
// the gateway guest uses for the control panel. This page is the recovery
// rendering of them: plain HTML forms that work with no JavaScript, no
// websocket, and no guest code involved.

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

async fn admin_branch(
    State(grip): State<Arc<Grip>>,
    axum::Form(form): axum::Form<BranchForm>,
) -> Html<String> {
    let banner = match crate::admin::act(&grip, &form.action, &form.target).await {
        Ok(message) => format!(r#"<p class="banner ok">{}</p>"#, html_escape(&message)),
        Err(e) => format!(
            r#"<p class="banner bad">{} failed: {}</p>"#,
            html_escape(&form.action),
            html_escape(&format!("{e:#}"))
        ),
    };
    Html(render_admin(&grip, &banner).await)
}

async fn admin_page(State(grip): State<Arc<Grip>>) -> Html<String> {
    Html(render_admin(&grip, "").await)
}

#[derive(serde::Deserialize)]
struct AdminUserLogout {
    user: String,
}

async fn admin_user_logout(
    State(grip): State<Arc<Grip>>,
    Form(form): Form<AdminUserLogout>,
) -> Response {
    match crate::admin::sign_out_everywhere(&grip, &form.user) {
        Ok(removed) => Redirect::to(&format!("/admin?signed_out={removed}")).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// One form button for an admin action, confirming first when the table says to.
fn action_form(action: &str, target: &str) -> String {
    let info = crate::admin::action(action);
    let label = info.map(|a| a.label).unwrap_or(action);
    let confirm = info
        .filter(|a| a.destructive)
        .map(|a| {
            format!(
                r#" onsubmit="return confirm('{}')""#,
                a.confirm.replace('\\', "\\\\").replace('\'', "\\'")
            )
        })
        .unwrap_or_default();
    format!(
        r#"<form method=post action="/admin/branch"{confirm}>
           <input type=hidden name=action value="{action}">
           <input type=hidden name=target value="{}">
           <button>{}</button></form>"#,
        html_escape(target),
        html_escape(label)
    )
}

async fn render_admin(grip: &Arc<Grip>, banner: &str) -> String {
    let view = crate::admin::overview(grip).await;

    let trunk_rows = view
        .commits
        .iter()
        .map(|c| {
            format!(
                "<tr><td class=mono>{}{}</td><td>{}</td><td class=note>{}</td><td>{}</td></tr>",
                &c.rev[..12.min(c.rev.len())],
                if c.head { " &larr; head" } else { "" },
                html_escape(&c.subject),
                html_escape(&c.author),
                if c.head {
                    String::new()
                } else {
                    action_form("trunk-reset", &c.rev)
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let branch_rows = view
        .branches
        .iter()
        .map(|row| {
            let actions = if row.live {
                action_form("stop-worker", &row.session_id)
                    + &action_form("abort-merge", &row.session_id)
            } else {
                action_form("release-worktree", &row.session_id)
            };
            format!(
                "<tr><td>{}</td><td class=mono>{}</td><td>{}</td><td class=mono>&uarr;{} &darr;{}</td>\
                 <td>{}</td><td class=mono>{}</td><td class=actions>{}</td></tr>\n",
                html_escape(&row.title),
                html_escape(&row.branch_ref),
                if row.live { "<span class=active>live</span>" } else { "stopped" },
                row.ahead,
                row.behind,
                row.state,
                row.kernel,
                actions
            )
        })
        .collect::<String>();

    let user_rows = view
        .accounts
        .iter()
        .map(|user| {
            let flags = [
                user.admin.then_some("admin"),
                user.read_only.then_some("read-only"),
                user.sees_all.then_some("sees all"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            format!(
                r#"<tr><td class=mono>{}</td><td>{}</td><td>{}</td><td class=note>{}</td><td>{}</td><td>{}</td><td>${:.4}</td><td class=actions><form method=post action="/admin/user/logout"><input type=hidden name=user value="{}"><button{}>sign out everywhere</button></form></td></tr>"#,
                html_escape(&user.id),
                html_escape(&user.name),
                html_escape(&user.role),
                html_escape(&flags),
                user.conversations,
                user.logins,
                user.spend_usd,
                html_escape(&user.id),
                if user.logins == 0 { " disabled" } else { "" },
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let private_list = if view.private_dirs.is_empty() {
        "<em>nothing</em>".to_string()
    } else {
        view.private_dirs
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
It keeps working when every guest and every worker is broken. The control panel in the
chat UI offers the same controls, and the configuration, when the guests are healthy.</p>
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
<h2>Users</h2>
<p class=note>Accounts are configuration (<code>[[users]]</code> in <code>thetis.local.toml</code>);
this is what the database says about them. "Sign out everywhere" ends every login the
account holds, on every device. Spend is cumulative across all of the account's conversations.</p>
<table><tr><th>user</th><th>name</th><th>role</th><th>policy</th><th>conversations</th><th>logins</th><th>spend (USD)</th><th></th></tr>
{user_rows}</table>
<h2>Publishing</h2>
<p class=note>Directories holding a <code>.thetis-private</code> marker never leave this
machine: a filtered <code>public</code> branch mirrors trunk without them, and a pre-push
hook refuses everything else. Currently private: {private_list}.</p>
{export_form}
{push_form}
<p class=note>When another checkout publishes too, pull before publishing: it merges what
they published into trunk here, so the next publish carries both instead of being refused
for replacing their work. Only paths that leave this machine are touched.</p>
{pull_form}
{adopt_form}
<p class=note>{sessions} session(s) on record.</p>
<p><a href="/">&larr; back to chat</a></p>"#,
        banner = banner,
        trunk_name = html_escape(&view.trunk_name),
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
        sessions = view.sessions,
        user_rows = if user_rows.is_empty() {
            "<tr><td colspan=8><em>local mode — one implicit administrator, no accounts</em></td></tr>".to_string()
        } else {
            user_rows
        },
        export_form = action_form("export-public", ""),
        push_form = action_form("push-public", ""),
        pull_form = action_form("pull-public", ""),
        adopt_form = action_form("adopt-remote", ""),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// --- websocket -------------------------------------------------------------

/// What the system is waiting on, as JSON. See `admin::waits`.
async fn admin_waits(State(grip): State<Arc<Grip>>) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&crate::admin::waits(&grip).await).unwrap_or_default(),
    )
        .into_response()
}

/// Serves `/` from a conversation's own gateway build.
async fn preview_root(
    State(grip): State<Arc<Grip>>,
    Extension(principal): Extension<Arc<crate::auth::Principal>>,
    Path(session): Path<String>,
) -> Response {
    if let Err(e) = crate::auth::may_access(&grip, &principal, &session) {
        return (StatusCode::FORBIDDEN, format!("{e:#}")).into_response();
    }
    preview_response(&grip, &session, "/").await
}

async fn preview_asset(
    State(grip): State<Arc<Grip>>,
    Extension(principal): Extension<Arc<crate::auth::Principal>>,
    Path((session, path)): Path<(String, String)>,
) -> Response {
    if let Err(e) = crate::auth::may_access(&grip, &principal, &session) {
        return (StatusCode::FORBIDDEN, format!("{e:#}")).into_response();
    }
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
                .into_response();
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
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(fallback_page(&detail)),
            )
                .into_response()
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
        let is_asset = url
            .rsplit('/')
            .next()
            .is_some_and(|last| last.contains('.'));
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

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(grip): State<Arc<Grip>>,
    Extension(principal): Extension<Arc<crate::auth::Principal>>,
) -> Response {
    // The protocol is small JSON frames; the one bulky payload is a
    // `workspace-write` of an edited text file (bounded well under this once
    // JSON-escaped), and raw bytes go over HTTP, not here. Capping the frame
    // keeps a client from buffering an unbounded message into the gateway.
    const MAX_WS_MESSAGE: usize = 16 * 1024 * 1024;
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| connection(socket, grip, principal))
}

/// How many outbound frames may be queued for one browser before the reader
/// starts waiting. A slow tab throttles itself; it never stalls the gateway.
const OUTBOUND_QUEUE: usize = 256;

async fn connection(socket: WebSocket, grip: Arc<Grip>, principal: Arc<crate::auth::Principal>) {
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
        grip.activity.subscribe(),
        watching.clone(),
        client_id.clone(),
        grip.clone(),
        principal.clone(),
    ));

    tracing::debug!(%client_id, "websocket connected");
    // Who this socket is for, before any guest frame. Host business like
    // `resync`: the guest has no `whoami` import and needs none.
    let _ = out_tx.send(user_frame(&principal)).await;

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

        // "Everyone's conversations" is a per-connection switch on the
        // principal, read by the host's `list_sessions` import. The frame then
        // goes on to the guest unchanged, which lists as it always did and
        // simply gets more rows. Someone without `see_all_sessions` can send
        // this all day: the switch is inert for them.
        if frame_type == "list" {
            if let Some(all) = frame.as_ref().and_then(|f| f.get("all")).and_then(|v| v.as_bool()) {
                principal.set_view_all(all);
                if all && !principal.may_see_all() {
                    let _ = out_tx
                        .send(error_frame(
                            "your role does not include seeing everyone's conversations",
                            Some("list".into()),
                        ))
                        .await;
                }
            }
        }

        // Host-side protocols do not pass through the gateway guest, so they
        // enforce ownership and capability policy here before dispatch.
        if let Some(frame) = &frame {
            let named_session = frame
                .get("id")
                .or_else(|| frame.get("session"))
                .and_then(serde_json::Value::as_str);
            if (crate::debug_api::handles(&frame_type) || crate::branch_api::handles(&frame_type))
                && named_session
                    .is_some_and(|id| crate::auth::may_access(&grip, &principal, id).is_err())
            {
                let _ = out_tx
                    .send(error_frame(
                        "that conversation is not yours",
                        Some(frame_type.clone()),
                    ))
                    .await;
                continue;
            }
            if crate::debug_api::handles(&frame_type)
                && matches!(frame_type.as_str(), "terminals" | "terminal-close")
                && principal.policy.denies(crate::policy::Cap::Terminal)
            {
                let _ = out_tx
                    .send(error_frame(
                        "terminal access is withheld by policy",
                        Some(frame_type.clone()),
                    ))
                    .await;
                continue;
            }
            if crate::branch_api::handles(&frame_type)
                && matches!(
                    frame_type.as_str(),
                    "branch-update"
                        | "branch-reset"
                        | "branch-resolve"
                        | "branch-abort"
                        | "branch-base"
                        | "branch-merge"
                )
                && principal.policy.denies(crate::policy::Cap::BranchWrite)
            {
                let _ = out_tx
                    .send(error_frame(
                        "branch changes are withheld by policy",
                        Some(frame_type.clone()),
                    ))
                    .await;
                continue;
            }
            if crate::workspace_api::handles(&frame_type) {
                let write = matches!(
                    frame_type.as_str(),
                    "workspace-write"
                        | "workspace-mkdir"
                        | "workspace-delete"
                        | "workspace-move"
                        | "workspace-rename"
                );
                let denied = principal.policy.denies(if write {
                    crate::policy::Cap::WorkspaceWrite
                } else {
                    crate::policy::Cap::Workspace
                });
                if denied {
                    let _ = out_tx
                        .send(error_frame(
                            "workspace access is withheld by policy",
                            Some(frame_type.clone()),
                        ))
                        .await;
                    continue;
                }
            }
        }

        // Inspection and turn control answer off to the side, concurrently.
        // These are the frames that must work *while* something else is slow —
        // the stop button most of all — and they are order-independent: they
        // read live worker state or cancel a turn, and never touch `watching`.
        if crate::debug_api::handles(&frame_type) || crate::system_api::handles(&frame_type) {
            let Some(frame) = frame else { continue };
            let grip = grip.clone();
            let out_tx = out_tx.clone();
            let principal = principal.clone();
            tokio::spawn(async move {
                let replies = if crate::debug_api::handles(&frame_type) {
                    crate::debug_api::handle(&grip, &frame).await
                } else {
                    crate::system_api::handle(&grip, &principal, &frame).await
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

        let actions =
            match gateway::on_client_message(&grip, &client_id, &text, principal.clone()).await {
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
                    let _ = out_tx.send(error_frame(&detail, inbound_type(&text))).await;
                    continue;
                }
            };

        for action in actions {
            match action {
                GatewayAction::Reply(frame) => {
                    let frame = decorate_sessions(&grip, &principal, frame);
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
                    // A subscription is what routes broadcast frames to this
                    // socket, and the host inserts whatever id the guest
                    // returned — so ownership is checked here, host-side,
                    // whatever the guest said.
                    match crate::auth::may_access(&grip, &principal, &session_id) {
                        Ok(()) => {
                            watching.write().await.insert(session_id);
                        }
                        Err(e) => {
                            tracing::warn!(user = %principal.user_id, session = %session_id, error = %e, "refused a subscription");
                            let _ = out_tx
                                .send(error_frame("that conversation is not yours", Some("open".into())))
                                .await;
                        }
                    }
                }
                GatewayAction::Unsubscribe(session_id) => {
                    if crate::auth::may_access(&grip, &principal, &session_id).is_ok() {
                        watching.write().await.remove(&session_id);
                    }
                }
            }
        }
    }

    drop(out_tx);
    writer.abort();
    tracing::debug!(%client_id, "websocket closed");
}

/// Decorates each row of a `sessions` frame with what the guest cannot know.
///
/// `SessionMeta` is a WIT record, so it cannot grow a field without changing
/// the contract every guest is matched against; these are added to the JSON on
/// the way out instead, which an older UI simply ignores.
///
/// - `activity`: the conversation's live state from `activity.rs` — working,
///   waiting, failed or idle, and the current step — so a freshly opened tab's
///   sidebar is right on first paint rather than after the next push.
/// - `owner`, `owner_name` and `mine`, only when this socket is showing
///   everyone's conversations: the guest does not know who is asking.
///
/// Any other frame passes through untouched.
fn decorate_sessions(grip: &Grip, principal: &crate::auth::Principal, frame: String) -> String {
    if !frame.contains("\"type\":\"sessions\"") {
        return frame;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&frame) else {
        return frame;
    };
    if value.get("type").and_then(|t| t.as_str()) != Some("sessions") {
        return frame;
    }
    let activity = grip.activity.all();
    if let Some(rows) = value.get_mut("sessions").and_then(|s| s.as_array_mut()) {
        for row in rows.iter_mut() {
            let Some(id) = row.get("id").and_then(|v| v.as_str()) else { continue };
            let Some(snap) = activity.get(id) else { continue };
            if let (Some(obj), Ok(snap)) = (row.as_object_mut(), serde_json::to_value(snap)) {
                obj.insert("activity".into(), snap);
            }
        }
    }
    if !principal.viewing_all() {
        return value.to_string();
    }
    let Some(owners) = grip.local_store().and_then(|s| s.owners_map().ok()) else {
        return value.to_string();
    };
    let name_of = |owner: &str| -> String {
        if let Some(user) = grip.cfg.auth.user(owner) {
            return user.name.clone();
        }
        match owner.strip_prefix("discord:") {
            Some(_) => "Discord".to_string(),
            None => owner.to_string(),
        }
    };
    if let Some(rows) = value.get_mut("sessions").and_then(|s| s.as_array_mut()) {
        for row in rows.iter_mut() {
            let Some(id) = row.get("id").and_then(|v| v.as_str()) else { continue };
            let Some(owner) = owners.get(id).cloned() else { continue };
            let mine = owner == principal.user_id;
            if let Some(obj) = row.as_object_mut() {
                obj.insert("mine".into(), serde_json::Value::Bool(mine));
                if !mine {
                    obj.insert("owner_name".into(), serde_json::Value::from(name_of(&owner)));
                    obj.insert("owner".into(), serde_json::Value::from(owner));
                }
            }
        }
    }
    value["everyone"] = serde_json::Value::Bool(true);
    value.to_string()
}

fn user_frame(principal: &crate::auth::Principal) -> String {
    let mut frame = principal.describe();
    frame["type"] = serde_json::Value::from("user");
    frame.to_string()
}

/// Owns the sink and nothing else. Moves bytes; never awaits a handler.
///
/// Three inputs: the connection's own replies, the rendered frames of the
/// conversations it is watching, and `activity` changes for *every*
/// conversation its principal may see — that last one is how the sidebar
/// learns that a conversation it is not looking at has started, stopped, or
/// is waiting on a question. Visibility is checked once per session and
/// remembered: ownership does not change, and a chatty turn would otherwise
/// cost a store read per step.
#[allow(clippy::too_many_arguments)]
async fn write_loop(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut out_rx: tokio::sync::mpsc::Receiver<String>,
    mut frames: tokio::sync::broadcast::Receiver<RenderedFrame>,
    mut activity: tokio::sync::broadcast::Receiver<crate::activity::Change>,
    watching: Arc<tokio::sync::RwLock<HashSet<String>>>,
    client_id: String,
    grip: Arc<Grip>,
    principal: Arc<crate::auth::Principal>,
) {
    let mut visible: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    loop {
        tokio::select! {
            queued = out_rx.recv() => {
                let Some(text) = queued else { return };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            change = activity.recv() => {
                match change {
                    Ok(change) => {
                        let may_see = *visible
                            .entry(change.session_id.clone())
                            .or_insert_with(|| {
                                crate::auth::may_access(&grip, &principal, &change.session_id).is_ok()
                            });
                        if may_see
                            && sink
                                .send(Message::Text(crate::activity::Activity::frame(&change).into()))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }
                    // Intermediate states are gone; the client's next
                    // `sessions` list carries the current ones. Nothing to
                    // rebuild, unlike a lagged event stream.
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
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
    use super::{authority_allowed, is_loopback_authority, is_loopback_origin, origin_allowed};
    use crate::config::Origin;

    fn public() -> Origin {
        Origin {
            scheme: "https".into(),
            authority: "thetis.example.com".into(),
        }
    }

    /// Local mode: no public origin, and the rule is exactly loopback-only.
    #[test]
    fn without_a_public_origin_only_loopback_is_allowed() {
        assert!(authority_allowed(None, "127.0.0.1:7777"));
        assert!(authority_allowed(None, "localhost"));
        assert!(!authority_allowed(None, "thetis.example.com"));
        assert!(origin_allowed(None, "http://localhost:7777"));
        assert!(!origin_allowed(None, "https://thetis.example.com"));
        assert!(!origin_allowed(None, "null"));
    }

    /// Users mode behind a proxy: the configured authority is admitted, and
    /// nothing else is — including the same name on another port, and the
    /// loopback rule is unchanged.
    #[test]
    fn the_public_origin_is_admitted_exactly() {
        let p = public();
        assert!(authority_allowed(Some(&p), "thetis.example.com"));
        assert!(!authority_allowed(Some(&p), "thetis.example.com:8443"));
        assert!(!authority_allowed(Some(&p), "evil.example.com"));
        assert!(!authority_allowed(Some(&p), "thetis.example.com.evil.com"));
        assert!(authority_allowed(Some(&p), "127.0.0.1:7777"), "loopback still works behind a proxy");
        assert!(origin_allowed(Some(&p), "https://thetis.example.com"));
        // The scheme is not part of the check: TLS is the proxy's business
        // and the origin guard is about *which site* the request is from.
        assert!(origin_allowed(Some(&p), "http://thetis.example.com"));
        assert!(!origin_allowed(Some(&p), "https://evil.example.com"));
        assert!(!origin_allowed(Some(&p), "null"));
    }

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
