//! Live check that a worker crash mid-turn heals: the supervisor respawns
//! the worker, the gateway repairs the log, and the interrupted turn is
//! carried on — all while the browser's socket stays connected.
//!
//! Ignored by default: it needs a running gateway+worker on the mock LLM,
//! and it *kills the worker process* on this machine. Run only against a
//! scratch instance:
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!     cargo test -p thetis --test ws_crash -- --ignored --nocapture

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn a_worker_crash_mid_turn_is_resumed() {
    let Some(url) = std::env::var("THETIS_WS_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        eprintln!("skipped: set THETIS_WS_URL (e.g. ws://127.0.0.1:7797/ws)");
        return;
    };

    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connecting to the gateway websocket");

    socket
        .send(Message::Text(
            r#"{"type":"new","title":"ws-crash smoke"}"#.into(),
        ))
        .await
        .unwrap();
    let session = wait_for(&mut socket, "a session id", 30, |f| {
        (f["type"] == "opened").then(|| f["session"].as_str().unwrap_or("").to_string())
    })
    .await;

    // "slow" makes the mock stream a word at a time, opening a window to die in.
    socket
        .send(Message::Text(
            serde_json::json!({ "type": "send", "id": session, "text": "please answer slow" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    wait_for(&mut socket, "the turn to start streaming", 30, |f| {
        (f["type"] == "event" && f["kind"] == "delta").then_some(())
    })
    .await;

    // Mid-stream: kill the worker out from under the turn.
    let killed = std::process::Command::new("pkill")
        .args(["-9", "-f", "thetis worker"])
        .status()
        .expect("running pkill");
    assert!(killed.success(), "there should have been a worker to kill");

    // The supervisor respawns the worker; its ready note makes the gateway
    // reconcile the log and resume the turn. (The interruption note itself is
    // written during reconciliation and shows on reload, not as a live frame.)
    // The resumed turn starting is the live signal that the loop closed.
    wait_for(&mut socket, "the resumed turn to start", 90, |f| {
        (f["type"] == "event" && f["kind"] == "turn-started").then_some(())
    })
    .await;

    let finished = wait_for(&mut socket, "the resumed turn to finish", 90, |f| {
        (f["type"] == "event" && (f["kind"] == "turn-finished" || f["kind"] == "incident"))
            .then(|| f["kind"] == "turn-finished")
    })
    .await;
    assert!(finished, "the resumed turn should finish, not raise an incident");
}

async fn wait_for<T>(
    socket: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + SinkExt<Message>
              + Unpin),
    what: &str,
    secs: u64,
    pick: impl Fn(&Value) -> Option<T>,
) -> T {
    let step = async {
        loop {
            let Some(Ok(message)) = socket.next().await else {
                panic!("socket closed while waiting for {what}");
            };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(found) = pick(&frame) {
                return found;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(secs), step)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
