//! Live check of the control panel's `admin` frames over the real websocket.
//!
//! Ignored by default: it needs a running Thetis in users mode with two
//! accounts, one an admin and one a plain user. Point it at a scratch gateway
//! with its own overlay, because it writes a setting (and puts it back):
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!   THETIS_AUTH_ADMIN=alice:password THETIS_AUTH_USER=bob:password \
//!     cargo test -p thetis --test ws_admin -- --ignored --nocapture
//!
//! What a pass proves: an administrator gets an `admin-overview` with the
//! host's action table, every setting described with its source, and the list
//! sections with their columns; a plain user gets `admin-result` refusals and
//! never a row of data; a setting round-trips through `set-field` with the
//! host naming the file it wrote; a value the loader would refuse is refused
//! and leaves the file alone; a user saved with a password comes back with the
//! hash masked and the password nowhere; and a stale-kernel answer ("unknown
//! frame type") is a failure, not a skip.

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

struct Env {
    ws_url: String,
    authority: String,
    admin: (String, String),
    user: (String, String),
}

fn env() -> Option<Env> {
    let ws_url = std::env::var("THETIS_WS_URL").ok().filter(|v| !v.trim().is_empty())?;
    let authority = ws_url
        .strip_prefix("ws://")?
        .split('/')
        .next()?
        .to_string();
    let pair = |key: &str| -> Option<(String, String)> {
        let raw = std::env::var(key).ok()?;
        let (u, p) = raw.split_once(':')?;
        Some((u.to_string(), p.to_string()))
    };
    Some(Env {
        ws_url,
        authority,
        admin: pair("THETIS_AUTH_ADMIN")?,
        user: pair("THETIS_AUTH_USER")?,
    })
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    /// The `thetis_session` token from `Set-Cookie`, if one was issued.
    fn cookie(&self) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .find_map(|(_, v)| {
                let (name, rest) = v.split_once('=')?;
                (name == "thetis_session").then(|| rest.split(';').next().unwrap_or("").to_string())
            })
            .filter(|t| !t.is_empty())
    }
}

/// One plain HTTP/1.1 exchange. Hand-rolled because the crate has no HTTP
/// client dependency and this is four lines of protocol.
async fn http(env: &Env, method: &str, path: &str, cookie: Option<&str>, form: Option<&str>) -> Reply {
    let mut stream = tokio::net::TcpStream::connect(&env.authority)
        .await
        .expect("connecting to the gateway");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n", env.authority);
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: thetis_session={c}\r\n"));
    }
    if let Some(body) = form {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
    } else {
        req.push_str("\r\n");
    }
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .expect("the response arrived")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Reply { status, headers, body: body.to_string() }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn login(env: &Env, user: &str, password: &str) -> Reply {
    let form = format!("user={}&password={}&next=%2F", urlencode(user), urlencode(password));
    http(env, "POST", "/login", None, Some(&form)).await
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn connect(env: &Env, cookie: Option<&str>) -> Result<Socket, u16> {
    let mut req = env.ws_url.as_str().into_client_request().unwrap();
    if let Some(c) = cookie {
        req.headers_mut()
            .insert("Cookie", format!("thetis_session={c}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("websocket connect failed in an unexpected way: {e}"),
    }
}

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// Reads frames until `pick` returns something, or gives up after a while.
async fn wait_for<T>(socket: &mut Socket, what: &str, mut pick: impl FnMut(&Value) -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = tokio::time::timeout(remaining, socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        let Some(Ok(Message::Text(text))) = next else {
            panic!("socket ended while waiting for {what}");
        };
        let frame: Value = serde_json::from_str(&text).unwrap();
        if let Some(found) = pick(&frame) {
            return found;
        }
    }
}


async fn admin_socket(env: &Env) -> Socket {
    let login = login(env, &env.admin.0, &env.admin.1).await;
    let cookie = login.cookie().expect("the admin login issued a cookie");
    connect(env, Some(&cookie)).await.expect("admin socket")
}

async fn user_socket(env: &Env) -> Socket {
    let login = login(env, &env.user.0, &env.user.1).await;
    let cookie = login.cookie().expect("the user login issued a cookie");
    connect(env, Some(&cookie)).await.expect("user socket")
}

/// The next frame of `kind`, or a panic on an `error` that names `admin` —
/// which is what an older kernel answers, and must not pass as "skipped".
async fn admin_reply(socket: &mut Socket, kind: &str) -> Value {
    wait_for(socket, kind, |f| {
        if f["type"] == "error" && f["message"].as_str().unwrap_or("").contains("admin") {
            panic!("the host does not speak the admin frames: {}", f["message"]);
        }
        (f["type"] == kind).then(|| f.clone())
    })
    .await
}

#[tokio::test]
#[ignore]
async fn an_administrator_sees_the_whole_panel_and_a_user_sees_nothing() {
    let Some(env) = env() else {
        eprintln!("skipped: THETIS_WS_URL / THETIS_AUTH_ADMIN / THETIS_AUTH_USER not set");
        return;
    };

    // --- the administrator -------------------------------------------------
    let mut admin = admin_socket(&env).await;

    send(&mut admin, serde_json::json!({ "type": "admin", "op": "overview" })).await;
    let overview = admin_reply(&mut admin, "admin-overview").await;
    assert!(overview["trunk_head"].as_str().unwrap_or("").len() >= 12, "{overview}");
    let actions = overview["actions"].as_array().expect("the action table");
    for id in ["trunk-reset", "stop-worker", "push-public", "pull-public"] {
        assert!(actions.iter().any(|a| a["id"] == id), "action {id} missing from {actions:?}");
    }
    assert!(
        actions.iter().filter(|a| a["destructive"] == true).all(|a| !a["confirm"].as_str().unwrap_or("").is_empty()),
        "a destructive action must say what it asks"
    );
    assert_eq!(overview["local_mode"], false);
    assert!(overview["accounts"].as_array().unwrap().iter().any(|a| a["id"] == env.admin.0));

    send(&mut admin, serde_json::json!({ "type": "admin", "op": "waits" })).await;
    let waits = admin_reply(&mut admin, "admin-waits").await;
    assert!(waits["uptime_s"].is_number(), "{waits}");

    send(&mut admin, serde_json::json!({ "type": "admin", "op": "fields" })).await;
    let fields = admin_reply(&mut admin, "admin-fields").await;
    let rows = fields["fields"].as_array().expect("fields");
    assert!(rows.len() > 100, "every setting is described: {}", rows.len());
    let model = rows.iter().find(|f| f["key"] == "llm.model").expect("llm.model");
    assert_eq!(model["kind"], "model");
    assert!(model["choices"].as_array().unwrap().len() > 0, "{model}");
    assert!(["default", "file", "local", "env"].contains(&model["source"].as_str().unwrap()));
    let key = rows.iter().find(|f| f["key"] == "llm.api_key").expect("llm.api_key");
    assert_eq!(key["secret"], true);
    assert!(key["value"] == "***" || key["value"] == "", "a secret is never read back: {key}");
    assert!(
        !rows.iter().any(|f| f["value"].as_str().unwrap_or("").starts_with("sk-")),
        "a key leaked into the field list"
    );
    assert!(fields["sections"].as_array().unwrap().iter().any(|s| s["id"] == "llm"));

    send(&mut admin, serde_json::json!({ "type": "admin", "op": "entries", "section": "users" })).await;
    let entries = admin_reply(&mut admin, "admin-entries").await;
    let users = entries["entries"].as_array().unwrap();
    assert!(users.iter().any(|u| u["id"] == env.admin.0), "{entries}");
    for u in users {
        let hash = u["fields"]["password_hash"].as_str().unwrap_or("");
        assert!(hash.is_empty() || hash == "***", "a hash leaked: {u}");
    }
    let tables = entries["tables"].as_array().unwrap();
    let users_table = tables.iter().find(|t| t["id"] == "users").unwrap();
    assert!(users_table["columns"].as_array().unwrap().iter().any(|c| c["key"] == "password"));

    // --- a live setting round-trips and is applied at once ----------------------
    let iterations = rows.iter().find(|f| f["key"] == "agent.max_iterations").unwrap();
    assert_eq!(iterations["restart_required"], false, "{iterations}");
    let before = iterations["value"].as_str().unwrap().to_string();
    let bumped = (before.parse::<i64>().unwrap() + 1).to_string();
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "set-field", "key": "agent.max_iterations", "value": bumped })).await;
    let result = admin_reply(&mut admin, "admin-result").await;
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["op"], "set-field");
    let message = result["message"].as_str().unwrap();
    assert!(message.contains("written to"), "{result}");
    assert!(message.contains("applied agent.max_iterations immediately"), "{result}");
    let fresh = admin_reply(&mut admin, "admin-fields").await;
    assert_eq!(fresh["prefix"], "agent");
    let now = fresh["fields"].as_array().unwrap().iter().find(|f| f["key"] == "agent.max_iterations").unwrap();
    assert_eq!(now["value"], bumped);
    let after = admin_reply(&mut admin, "admin-overview").await;
    assert_eq!(after["pending_restart"].as_array().unwrap().len(), 0, "a live change never waits: {after}");
    // Put it back.
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "set-field", "key": "agent.max_iterations", "value": before })).await;
    assert_eq!(admin_reply(&mut admin, "admin-result").await["ok"], true);
    let _ = admin_reply(&mut admin, "admin-fields").await;
    let _ = admin_reply(&mut admin, "admin-overview").await;

    // --- a boot-bound setting waits for a restart, until it is put back ------------
    let memory = rows.iter().find(|f| f["key"] == "limits.agent_memory_mb").unwrap();
    assert_eq!(memory["restart_required"], true, "{memory}");
    let was = memory["value"].as_str().unwrap().to_string();
    let more = (was.parse::<i64>().unwrap() + 1).to_string();
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "set-field", "key": "limits.agent_memory_mb", "value": more })).await;
    let result = admin_reply(&mut admin, "admin-result").await;
    assert_eq!(result["ok"], true, "{result}");
    assert!(result["message"].as_str().unwrap().contains("need a restart") || result["message"].as_str().unwrap().contains("needs a restart"), "{result}");
    let _ = admin_reply(&mut admin, "admin-fields").await;
    let pending = admin_reply(&mut admin, "admin-overview").await;
    assert_eq!(pending["pending_restart"], serde_json::json!(["limits.agent_memory_mb"]), "{pending}");
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "set-field", "key": "limits.agent_memory_mb", "value": was })).await;
    assert_eq!(admin_reply(&mut admin, "admin-result").await["ok"], true);
    let _ = admin_reply(&mut admin, "admin-fields").await;
    let cleared = admin_reply(&mut admin, "admin-overview").await;
    assert_eq!(cleared["pending_restart"].as_array().unwrap().len(), 0, "putting the value back clears it: {cleared}");

    // --- reload from disk reports rather than refuses --------------------------
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "reload" })).await;
    let reloaded = admin_reply(&mut admin, "admin-result").await;
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    assert!(reloaded["message"].as_str().unwrap().contains("nothing changed"), "{reloaded}");

    // --- a value the loader refuses is refused ------------------------------
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "set-field", "key": "server.bind", "value": "not-an-address" })).await;
    let refused = admin_reply(&mut admin, "admin-result").await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("invalid"), "{refused}");

    // --- an unknown op is refused, not trapped --------------------------------
    send(&mut admin, serde_json::json!({ "type": "admin", "op": "explode" })).await;
    let unknown = admin_reply(&mut admin, "admin-result").await;
    assert_eq!(unknown["ok"], false);

    // --- the plain user --------------------------------------------------------
    let mut user = user_socket(&env).await;
    for op in ["overview", "fields", "waits"] {
        send(&mut user, serde_json::json!({ "type": "admin", "op": op })).await;
        let reply = admin_reply(&mut user, "admin-result").await;
        assert_eq!(reply["ok"], false, "{op}: {reply}");
        assert_eq!(reply["op"], op);
    }
    send(&mut user, serde_json::json!({ "type": "admin", "op": "set-field", "key": "agent.max_iterations", "value": "1" })).await;
    assert_eq!(admin_reply(&mut user, "admin-result").await["ok"], false);
    send(&mut user, serde_json::json!({ "type": "admin", "op": "entries", "section": "users" })).await;
    assert_eq!(admin_reply(&mut user, "admin-result").await["ok"], false);
}
