//! Live check of multi-party conversations: the participants roster, the
//! invite and remove frames, cross-account visibility, and per-speaker
//! authority as the browser actually sees it.
//!
//! Ignored by default: it needs a running Thetis in users mode with two
//! accounts, one an admin and one a **read-only** plain user. Run with
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!   THETIS_AUTH_ADMIN=alice:alicepw THETIS_AUTH_USER=bob:bobpw \
//!     cargo test -p thetis --test ws_participants -- --ignored --nocapture
//!
//! **Why this exists and the unit tests are not enough.** Everything below the
//! WIT contract is already covered: `store.rs` tests the participants table,
//! and `persist.rs` tests per-speaker policy resolution across a real IPC pair.
//! What no unit test can reach is the *contract itself* — `participants`,
//! `add-participant`, `remove-participant` and `invitable-accounts` are host
//! imports, so they only exist once a kernel serves them to a guest that was
//! built against the same WIT. A signature mismatch, a missing linker entry, or
//! a record whose fields the guest decodes differently all pass every unit test
//! and fail here. That gap is exactly what `rebuild rejected … function
//! implementation is missing` was, and it is only visible live.
//!
//! What a pass proves, in order:
//!   1. the roster answers over the wire and names the owner as a participant
//!   2. `invitable-accounts` is owner-only, and is not an account directory
//!   3. an invitation makes the conversation appear in the invitee's sidebar
//!   4. the roster reports each participant's *own* read-only standing, which
//!      is the only place the per-speaker rule is visible to a reader
//!   5. a participant may open and read the conversation
//!   6. a non-owner cannot invite, and cannot evict a third party
//!   7. anyone may remove themselves
//!   8. removal takes the conversation back out of the sidebar and locks it

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
    third: Option<(String, String)>,
}

fn env() -> Option<Env> {
    let ws_url = std::env::var("THETIS_WS_URL").ok().filter(|v| !v.trim().is_empty())?;
    let authority = ws_url.strip_prefix("ws://")?.split('/').next()?.to_string();
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
        // Optional. Two accounts prove sharing works; the third proves one
        // guest cannot evict another, which is the case the owner-or-self rule
        // actually exists for. Trying it with the owner as the target proves
        // nothing either way — the owner holds no participant row, so removing
        // them is a no-op even with no guard at all.
        third: pair("THETIS_AUTH_THIRD"),
    })
}

struct Reply {
    headers: Vec<(String, String)>,
}

impl Reply {
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
    let body = format!("user={}&password={}&next=%2F", urlencode(user), urlencode(password));
    let mut stream = tokio::net::TcpStream::connect(&env.authority).await.expect("connect");
    let req = format!(
        "POST /login HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        env.authority,
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .expect("a response")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let head = text.split_once("\r\n\r\n").map(|(h, _)| h).unwrap_or(&text);
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Reply { headers }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(env: &Env, cookie: &str) -> Socket {
    let mut req = env.ws_url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("Cookie", format!("thetis_session={cookie}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(req).await.expect("a socket");
    socket
}

async fn send(socket: &mut Socket, frame: Value) {
    socket.send(Message::Text(frame.to_string().into())).await.unwrap();
}

async fn wait_for<T>(
    socket: &mut Socket,
    what: &str,
    mut pick: impl FnMut(&Value) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = tokio::time::timeout(remaining, socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        let Some(Ok(Message::Text(text))) = next else {
            panic!("the socket ended while waiting for {what}");
        };
        let frame: Value = serde_json::from_str(&text).unwrap();
        if let Some(found) = pick(&frame) {
            return found;
        }
    }
}

/// Waits for the next `participants` frame, or the refusal of one.
async fn roster(socket: &mut Socket) -> Result<Value, String> {
    wait_for(socket, "a participants frame", |f| match f["type"].as_str() {
        Some("participants") => Some(Ok(f.clone())),
        Some("error") => Some(Err(f["message"].as_str().unwrap_or("").to_string())),
        _ => None,
    })
    .await
}

async fn ask_roster(socket: &mut Socket, id: &str) -> Result<Value, String> {
    send(socket, serde_json::json!({ "type": "participants", "id": id })).await;
    roster(socket).await
}

async fn listed(socket: &mut Socket) -> Vec<String> {
    send(socket, serde_json::json!({ "type": "list" })).await;
    wait_for(socket, "a sessions frame", |f| {
        (f["type"] == "sessions").then(|| {
            f["sessions"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .filter_map(|x| x["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        })
    })
    .await
}

/// Opens an existing conversation, returning its id or the refusal.
///
/// `open` replies with a `history` frame carrying the transcript; there is no
/// `opened` frame on this path (only `new` sends one), so that is what a
/// success looks like.
async fn opened_by(socket: &mut Socket, id: &str) -> Result<String, String> {
    send(socket, serde_json::json!({ "type": "open", "id": id })).await;
    wait_for(socket, "a history frame or a refusal", |f| match f["type"].as_str() {
        Some("history") => Some(Ok(f["session"].as_str().unwrap_or("").to_string())),
        Some("error") => Some(Err(f["message"].as_str().unwrap_or("").to_string())),
        _ => None,
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

/// The accounts on a roster, as `(account, owner, read_only)`.
fn people(frame: &Value) -> Vec<(String, bool, bool)> {
    frame["participants"]
        .as_array()
        .expect("a participants array")
        .iter()
        .map(|p| {
            (
                p["account"].as_str().unwrap_or("").to_string(),
                p["owner"].as_bool().unwrap_or(false),
                p["read_only"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

fn invitable(frame: &Value) -> Vec<String> {
    frame["invitable"]
        .as_array()
        .expect("an invitable array")
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
#[ignore]
async fn a_conversation_can_be_shared_and_unshared() {
    let Some(env) = env() else {
        eprintln!(
            "skipped: set THETIS_WS_URL, THETIS_AUTH_ADMIN=user:pw and THETIS_AUTH_USER=user:pw"
        );
        return;
    };
    let owner_id = env.admin.0.to_lowercase();
    let guest_id = env.user.0.to_lowercase();

    let admin_cookie = login(&env, &env.admin.0, &env.admin.1)
        .await
        .cookie()
        .expect("the admin logged in");
    let user_cookie = login(&env, &env.user.0, &env.user.1)
        .await
        .cookie()
        .expect("the user logged in");

    let mut a = connect(&env, &admin_cookie).await;
    let mut b = connect(&env, &user_cookie).await;
    for s in [&mut a, &mut b] {
        wait_for(s, "the user frame", |f| (f["type"] == "user").then_some(())).await;
    }

    let convo = open_new(&mut a, "ws-participants shared").await;
    assert!(!convo.is_empty());

    // --- 1. the roster answers, and the owner is on it -----------------------
    //
    // The owner holds no participant row — ownership is its own table — so a
    // roster that omitted them would be technically accurate and useless: it
    // is what tells a reader whose conversation this is, and therefore whether
    // the invite and remove controls belong to them.
    let r = ask_roster(&mut a, &convo).await.expect("the owner gets a roster");
    assert_eq!(r["session"], convo.as_str());
    let seats = people(&r);
    assert_eq!(seats.len(), 1, "just the owner to begin with: {seats:?}");
    assert_eq!(seats[0].0, owner_id);
    assert!(seats[0].1, "the owner is flagged as such");

    // --- 2. invitable-accounts offers the other account ---------------------
    let offered = invitable(&r);
    assert!(offered.contains(&guest_id), "the other account can be invited: {offered:?}");
    assert!(!offered.contains(&owner_id), "you cannot invite yourself");

    // Someone with no connection to the conversation is refused the *frame*,
    // before `invitable-accounts` is ever consulted — `may_access` gates the
    // whole dispatch. So the "answers empty rather than refusing" design only
    // ever applies to a participant who is not the owner, which is checked at
    // step 4 once the guest is actually in. Asserting it here would have been
    // testing an unreachable path.
    let refused = ask_roster(&mut b, &convo).await;
    assert!(
        refused.is_err(),
        "a stranger is refused the roster outright, not handed an empty one: {refused:?}"
    );

    // --- 3. an invitation reaches the invitee's sidebar ----------------------
    //
    // Not cosmetic: the sidebar is the only way to reach a conversation, so
    // sharing something that never appears in it is not sharing it.
    assert!(!listed(&mut b).await.contains(&convo), "not yet shared");
    send(
        &mut a,
        serde_json::json!({ "type": "participant-add", "id": convo, "account": guest_id }),
    )
    .await;
    let after = roster(&mut a).await.expect("the invite is confirmed with a roster");
    let seats = people(&after);
    assert_eq!(seats.len(), 2, "owner and guest: {seats:?}");
    assert!(listed(&mut b).await.contains(&convo), "the invitee can now see it");

    // --- 4. the roster shows each person's own standing ----------------------
    //
    // The transcript looks identical whoever speaks, so this is the only place
    // the per-speaker rule is visible to a human. `effective = policy(speaker)
    // ∩ ceiling`, and the read-only account stays read-only in an admin's
    // conversation — which is the property the whole design was built for.
    let guest_seat = seats.iter().find(|(id, _, _)| id == &guest_id).expect("the guest");
    assert!(
        guest_seat.2,
        "the read-only account must be shown as read-only here, not inherit the owner's write access"
    );
    let owner_seat = seats.iter().find(|(id, _, _)| id == &owner_id).expect("the owner");
    assert!(!owner_seat.2, "the owner is not read-only");

    // Now that the guest is in, the real non-owner case is reachable: they get
    // the roster (so they can see who else is here) but an *empty* invitable
    // list, which is what hides the invite control rather than offering one
    // that would be refused. It is also not an account directory — only the
    // owner learns which accounts exist.
    let theirs = ask_roster(&mut b, &convo).await.expect("a participant sees the roster");
    assert_eq!(people(&theirs).len(), 2, "the guest sees both seats");
    assert!(
        invitable(&theirs).is_empty(),
        "a participant who is not the owner gets no account list: {:?}",
        invitable(&theirs)
    );

    // --- 5. a participant may open and read it ------------------------------
    //
    // `open` answers with the transcript, not an `opened` frame — only `new`
    // sends that. Waiting for the wrong one here looked exactly like the
    // gateway ignoring a participant's open, which is worth remembering: the
    // absence of a frame is ambiguous between "refused" and "answered
    // differently", so both are named in the pick below.
    assert_eq!(
        opened_by(&mut b, &convo).await,
        Ok(convo.clone()),
        "a participant may open the conversation and read its history"
    );

    // --- 6. a non-owner can neither invite nor evict a third party ----------
    //
    // Removal used to be guarded only by "the call came from the gateway", so
    // any participant could evict the others — including to hide what they had
    // been shown from the person who shared it.
    send(
        &mut b,
        serde_json::json!({ "type": "participant-add", "id": convo, "account": owner_id }),
    )
    .await;
    let refused = wait_for(&mut b, "a refusal of the invite", |f| match f["type"].as_str() {
        Some("error") => Some(true),
        // A participants frame would mean it went through.
        Some("participants") => Some(false),
        _ => None,
    })
    .await;
    assert!(refused, "a non-owner must not be able to invite");
    // The refusal is followed by a refreshed roster; take it off the queue.
    let _ = roster(&mut b).await;

    // --- 6b. nor evict the owner, nor retire the conversation ---------------
    //
    // The same missing-guard class as the invite above. Removal is "the owner
    // may remove anyone, anyone may remove themselves", and archiving is the
    // owner's alone: it stops the worker and releases the worktree, so a guest
    // who could archive could retire a conversation out from under the person
    // who shared it.
    //
    // Assert on the roster rather than on which frame comes back. A refusal
    // replies `error` *and* a fresh `participants`, so matching frame types
    // here reads whichever arrives first — and a stale `participants` left
    // over from the previous refusal looks exactly like success. What the
    // store holds afterwards is the thing that actually matters.
    if let Some((third_user, third_pw)) = env.third.clone() {
        let third_id = third_user.to_lowercase();
        // The owner invites a second guest, so there are now two people in the
        // room who are each other's equals. Neither may evict the other.
        send(
            &mut a,
            serde_json::json!({ "type": "participant-add", "id": convo, "account": third_id }),
        )
        .await;
        // `roster`, not `ask_roster`: a mutation already replies with a
        // participants frame. Asking for another leaves a second frame queued,
        // and the next read picks up that stale one — which is how a later
        // assertion can pass against a roster from before the change it is
        // meant to be checking.
        let seated = roster(&mut a).await.expect("a roster after the second invite");
        assert!(
            people(&seated).iter().any(|(account, _, _)| account == &third_id),
            "the second guest was seated"
        );

        send(
            &mut b,
            serde_json::json!({ "type": "participant-remove", "id": convo, "account": third_id }),
        )
        .await;
        // Drain b's own reply, so nothing is left queued for later steps. A
        // refusal answers with `error` *and* a refreshed `participants`, so
        // both have to come off the queue — draining one leaves a frame that a
        // later step reads as the answer to its own request, and the failure
        // then lands several assertions away from its cause.
        let bs_view = roster(&mut b).await;
        if bs_view.is_err() {
            let _ = roster(&mut b).await;
        }

        // The evidence is the third account's own access, read on their own
        // socket — not a roster frame. Rosters are requested and answered
        // asynchronously, so asking the owner "is carol still listed?" right
        // after b's attempt can be served before b's write commits, and reads
        // as untouched whether or not the guard exists. That race is not
        // hypothetical: it made this very assertion pass against a guard I had
        // deliberately deleted. Logging in as the person who would have been
        // evicted cannot race, because it happens strictly afterwards and asks
        // the store the question that actually matters.
        let third_cookie = login(&env, &third_user, &third_pw)
            .await
            .cookie()
            .expect("the third account logged in");
        let mut c = connect(&env, &third_cookie).await;
        wait_for(&mut c, "the user frame", |f| (f["type"] == "user").then_some(())).await;
        assert!(
            listed(&mut c).await.contains(&convo),
            "one guest must not be able to evict another: {third_id} lost access \
             when {guest_id} asked for their removal (b was told {bs_view:?})"
        );
        assert!(
            opened_by(&mut c, &convo).await.is_ok(),
            "and can still open it"
        );
        let _ = c.close(None).await;

        // Tidy up, so the roster assertions after this step still read 1.
        // Asserted rather than best-effort: a silently-failed cleanup shows up
        // as a confusing count mismatch several steps later.
        send(
            &mut a,
            serde_json::json!({ "type": "participant-remove", "id": convo, "account": third_id }),
        )
        .await;
        // A removal replies with the sidebar; ask for the roster separately,
        // as the People panel does when it was somebody else who left.
        let tidied = wait_for(&mut a, "the sidebar after the removal", |f| {
            (f["type"] == "sessions").then_some(())
        })
        .await;
        let _ = tidied;
        let seats = people(&ask_roster(&mut a, &convo).await.expect("a roster"));
        assert!(
            !seats.iter().any(|(account, _, _)| account == &third_id),
            "the second guest was removed; roster {seats:?}"
        );
        assert_eq!(seats.len(), 2, "the owner and the first guest remain");
    }

    send(
        &mut b,
        serde_json::json!({ "type": "archive", "id": convo, "archived": true }),
    )
    .await;
    // Drain b's reply before asserting, or the refusal sits in b's queue and
    // the next step reads it as the answer to *its* request. Then check the
    // owner's sidebar, which is the evidence: an archived conversation drops
    // out of it.
    let archive_reply = wait_for(&mut b, "the result of archiving", |f| {
        match f["type"].as_str() {
            Some("error") => Some(Err(f["message"].as_str().unwrap_or("").to_string())),
            Some("sessions") => Some(Ok(())),
            _ => None,
        }
    })
    .await;
    // Here the reply *is* the evidence, and unusually it is the reliable kind:
    // `archive` answers with `sessions` on success and `error` on refusal, so
    // the two cases are distinguishable with no race. Checking the owner's
    // sidebar as well would be the durable check, but it lags — a successful
    // archive is not reflected in another connection's next `list` — so it
    // reports a stale pass. Assert the refusal, then confirm the conversation
    // is still usable, which is what an archive would have taken away.
    assert!(
        archive_reply.is_err(),
        "a guest must not be able to archive the owner's conversation; \
         b was told {archive_reply:?}"
    );
    assert!(
        listed(&mut a).await.contains(&convo),
        "and the owner still has it"
    );

    // --- 7. anyone may leave -------------------------------------------------
    //
    // The one asymmetry in the owner-or-self rule: leaving needs nobody's
    // permission.
    send(
        &mut b,
        serde_json::json!({ "type": "participant-remove", "id": convo, "account": guest_id }),
    )
    .await;
    // Answered with the sidebar, not a roster: the reader has just ended their
    // own access, so a roster is no longer theirs to read and asking for one
    // would be refused — which, for a guest import, means trapping in the
    // middle of a mutation that had already succeeded.
    let left = wait_for(&mut b, "the result of leaving", |f| match f["type"].as_str() {
        Some("sessions") => Some(Ok(())),
        Some("error") => Some(Err(f["message"].as_str().unwrap_or("").to_string())),
        _ => None,
    })
    .await;
    assert!(left.is_ok(), "a participant may remove themselves: {left:?}");

    // --- 8. and the conversation is gone from their sidebar and locked ------
    assert!(
        !listed(&mut b).await.contains(&convo),
        "leaving takes it out of the sidebar again"
    );
    assert!(
        opened_by(&mut b, &convo).await.is_err(),
        "access ends with the invitation"
    );

    // The owner still has it, with the roster back to one seat.
    let back = ask_roster(&mut a, &convo).await.expect("the owner still has their roster");
    assert_eq!(people(&back).len(), 1, "the owner is unaffected");
    assert!(listed(&mut a).await.contains(&convo));

    let _ = a.close(None).await;
    let _ = b.close(None).await;
}
