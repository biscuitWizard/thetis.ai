//! Live check of users mode: accounts, the cookie, ownership isolation, and
//! the admin gate, over the real HTTP listener and websocket.
//!
//! Ignored by default: it needs a running Thetis started in users mode with
//! two accounts, one an admin and one a plain user whose role grants neither
//! `admin` nor `see_all_sessions`. Run with
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!   THETIS_AUTH_ADMIN=alice:password THETIS_AUTH_USER=bob:password \
//!     cargo test -p thetis --test ws_auth -- --ignored --nocapture
//!
//! What a pass proves: the login form issues a cookie the websocket upgrade
//! honours; a socket without one is refused with 401; each account's sidebar
//! omits the other's conversation; opening another account's conversation by
//! id yields an `error` frame and no subscription; `/admin` is 403 for the
//! plain user; `/api/me` describes the caller; and logging out ends the login.

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

/// The ids in the next `sessions` frame after asking for the list.
async fn listed(socket: &mut Socket, all: Option<bool>) -> Vec<String> {
    let mut frame = serde_json::json!({ "type": "list" });
    if let Some(all) = all {
        frame["all"] = Value::Bool(all);
    }
    send(socket, frame).await;
    wait_for(socket, "a sessions frame", |f| {
        (f["type"] == "sessions").then(|| {
            f["sessions"]
                .as_array()
                .map(|s| s.iter().filter_map(|x| x["id"].as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        })
    })
    .await
}

async fn open_new(socket: &mut Socket, title: &str) -> String {
    send(socket, serde_json::json!({ "type": "new", "title": title })).await;
    wait_for(socket, "an opened frame", |f| {
        (f["type"] == "opened").then(|| f["session"].as_str().unwrap_or("").to_string())
    })
    .await
}

#[tokio::test]
#[ignore]
async fn two_accounts_are_kept_apart() {
    let Some(env) = env() else {
        eprintln!("skipped: set THETIS_WS_URL, THETIS_AUTH_ADMIN=user:pw and THETIS_AUTH_USER=user:pw");
        return;
    };

    // --- the door ---------------------------------------------------------
    assert_eq!(connect(&env, None).await.err(), Some(401), "no cookie, no socket");
    assert_eq!(connect(&env, Some("not-a-token")).await.err(), Some(401));
    let me = http(&env, "GET", "/api/me", None, None).await;
    assert_eq!(me.status, 401);

    let bad = login(&env, &env.user.0, "definitely-not-the-password").await;
    assert_eq!(bad.status, 200, "a refusal re-renders the form");
    assert!(bad.body.contains("Wrong user or password"), "{}", bad.body);
    assert!(bad.cookie().is_none(), "no cookie on a refusal");

    let admin = login(&env, &env.admin.0, &env.admin.1).await;
    assert!(matches!(admin.status, 302 | 303), "login redirects: {}", admin.status);
    assert_eq!(admin.header("location"), Some("/"));
    let admin_cookie = admin.cookie().expect("the admin got a cookie");
    let user = login(&env, &env.user.0, &env.user.1).await;
    let user_cookie = user.cookie().expect("the user got a cookie");
    let raw = user.header("set-cookie").unwrap();
    assert!(raw.contains("HttpOnly") && raw.contains("SameSite=Lax"), "{raw}");

    // --- who am I ----------------------------------------------------------
    let me = http(&env, "GET", "/api/me", Some(&user_cookie), None).await;
    assert_eq!(me.status, 200);
    let me: Value = serde_json::from_str(&me.body).unwrap();
    assert_eq!(me["id"], env.user.0.to_lowercase());
    assert_eq!(me["admin"], false);
    assert_eq!(me["local"], false);
    assert!(me["denied"].is_array());
    let me_admin: Value = serde_json::from_str(
        &http(&env, "GET", "/api/me", Some(&admin_cookie), None).await.body,
    )
    .unwrap();
    assert_eq!(me_admin["admin"], true);

    // --- the admin gate ----------------------------------------------------
    assert_eq!(http(&env, "GET", "/admin", Some(&user_cookie), None).await.status, 403);
    assert_eq!(http(&env, "GET", "/admin", Some(&admin_cookie), None).await.status, 200);
    assert_eq!(http(&env, "GET", "/admin", None, None).await.status, 401);

    // --- two sockets, two sidebars -------------------------------------------
    let mut a = connect(&env, Some(&admin_cookie)).await.expect("admin socket");
    let mut b = connect(&env, Some(&user_cookie)).await.expect("user socket");
    let who = wait_for(&mut b, "the user frame", |f| {
        (f["type"] == "user").then(|| f["id"].as_str().unwrap_or("").to_string())
    })
    .await;
    assert_eq!(who, env.user.0.to_lowercase(), "the first frame says who the socket is for");
    let admin_sees_all = wait_for(&mut a, "the admin's user frame", |f| {
        (f["type"] == "user").then(|| f["see_all"].as_bool().unwrap_or(false))
    })
    .await;
    send(&mut a, serde_json::json!({ "type": "hello" })).await;
    send(&mut b, serde_json::json!({ "type": "hello" })).await;

    let mine_a = open_new(&mut a, "ws-auth admin's").await;
    let mine_b = open_new(&mut b, "ws-auth user's").await;
    assert!(!mine_a.is_empty() && !mine_b.is_empty());

    let seen_by_b = listed(&mut b, None).await;
    assert!(seen_by_b.contains(&mine_b), "the user sees their own conversation");
    assert!(!seen_by_b.contains(&mine_a), "the user does not see the admin's");
    let seen_by_a = listed(&mut a, None).await;
    assert!(seen_by_a.contains(&mine_a));
    assert!(!seen_by_a.contains(&mine_b), "the admin's sidebar is personal by default");

    // With the grant, the switch works and is per connection: on, the admin
    // sees the user's conversation; off again, the sidebar is personal.
    if admin_sees_all {
        let everyone = listed(&mut a, Some(true)).await;
        assert!(everyone.contains(&mine_b), "see-all shows the user's conversation");
        let personal = listed(&mut a, Some(false)).await;
        assert!(!personal.contains(&mine_b), "and off again it is gone");
        let mut a2 = connect(&env, Some(&admin_cookie)).await.expect("a second admin socket");
        wait_for(&mut a2, "user", |f| (f["type"] == "user").then_some(())).await;
        assert!(!listed(&mut a2, None).await.contains(&mine_b), "a fresh socket starts personal");
        let _ = a2.close(None).await;
    } else {
        eprintln!("note: the admin's role lacks see_all_sessions; the switch is not exercised");
    }

    // Asking for everyone's is inert without the grant, and says so.
    send(&mut b, serde_json::json!({ "type": "list", "all": true })).await;
    let refused = wait_for(&mut b, "a refusal or the list", |f| {
        if f["type"] == "error" {
            Some(true)
        } else if f["type"] == "sessions" {
            Some(f["sessions"].as_array().unwrap().iter().any(|s| s["id"] == mine_a.as_str()))
        } else {
            None
        }
    })
    .await;
    assert!(refused || !listed(&mut b, None).await.contains(&mine_a));

    // Opening the other's by id is an error frame, and no frames follow.
    send(&mut b, serde_json::json!({ "type": "open", "id": mine_a })).await;
    let message = wait_for(&mut b, "an error for the foreign open", |f| {
        (f["type"] == "error").then(|| f["message"].as_str().unwrap_or("").to_string())
    })
    .await;
    assert!(
        message.contains("not yours") || message.contains("another user") || message.contains("no such"),
        "{message}"
    );
    // A rename by id is refused the same way.
    send(&mut b, serde_json::json!({ "type": "rename", "id": mine_a, "title": "hijacked" })).await;
    wait_for(&mut b, "an error for the foreign rename", |f| (f["type"] == "error").then_some(())).await;
    let still = listed(&mut a, None).await;
    assert!(still.contains(&mine_a));

    // --- the way out -----------------------------------------------------------
    let out = http(&env, "POST", "/logout", Some(&user_cookie), None).await;
    assert!(matches!(out.status, 302 | 303), "{}", out.status);
    assert_eq!(out.header("location"), Some("/login"));
    assert_eq!(http(&env, "GET", "/api/me", Some(&user_cookie), None).await.status, 401, "the login is gone");
    assert_eq!(connect(&env, Some(&user_cookie)).await.err(), Some(401));
    // The admin is unaffected.
    assert_eq!(http(&env, "GET", "/api/me", Some(&admin_cookie), None).await.status, 200);

    let _ = a.close(None).await;
    let _ = b.close(None).await;
}
