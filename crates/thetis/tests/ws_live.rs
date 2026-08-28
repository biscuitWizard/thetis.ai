//! Live end-to-end check of the gateway/worker split over the real websocket.
//!
//! Ignored by default: it needs a running Thetis (gateway + worker) and an
//! LLM behind it — the mock is enough. Run with
//!   cargo run --bin mock-llm &
//!   THETIS_ROOT=<root> cargo run --bin thetis &
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!     cargo test -p thetis --test ws_live -- --ignored --nocapture
//!
//! It drives the same wire protocol the browser does: hello, new session,
//! send a message, then waits for the turn to produce rendered frames. A pass
//! proves the whole chain: gateway WS → guest dispatch → submit RPC → worker
//! session actor → turn against the LLM → events persisted over RPC → frames
//! rendered in the worker → shipped back → fanned out to this socket.

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn ws_url() -> Option<String> {
    std::env::var("THETIS_WS_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

#[tokio::test]
#[ignore]
async fn a_message_round_trips_through_gateway_and_worker() {
    let Some(url) = ws_url() else {
        eprintln!("skipped: set THETIS_WS_URL (e.g. ws://127.0.0.1:7797/ws)");
        return;
    };

    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connecting to the gateway websocket");

    // hello → catalog + sessions.
    socket
        .send(Message::Text(r#"{"type":"hello"}"#.into()))
        .await
        .unwrap();

    // A fresh conversation, so the assertion cannot ride on old history.
    socket
        .send(Message::Text(
            r#"{"type":"new","title":"ws-live smoke"}"#.into(),
        ))
        .await
        .unwrap();

    let session = wait_for(&mut socket, "a session id", |frame| {
        (frame["type"] == "opened").then(|| frame["session"].as_str().unwrap_or("").to_string())
    })
    .await;
    assert!(!session.is_empty(), "the new session came back with an id");

    socket
        .send(Message::Text(
            serde_json::json!({ "type": "send", "id": session, "text": "hello from the smoke test" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // The turn's progress arrives as rendered event frames. Seeing the user
    // message come back proves persistence (worker → gateway RPC) and the
    // worker-side renderer; seeing the turn finish proves the actor ran a
    // whole turn against the model.
    let user_echoed = wait_for(&mut socket, "the user message frame", |frame| {
        (frame["type"] == "event"
            && frame["kind"] == "user"
            && frame["text"]
                .as_str()
                .is_some_and(|t| t.contains("smoke test")))
        .then_some(true)
    })
    .await;
    assert!(user_echoed);

    let finished = wait_for(&mut socket, "the end of the turn", |frame| {
        (frame["type"] == "event"
            && (frame["kind"] == "turn-finished" || frame["kind"] == "incident"))
        .then(|| frame["kind"] == "turn-finished")
    })
    .await;
    assert!(finished, "the turn should finish rather than end in an incident");
}

/// Reads frames until `pick` accepts one, failing loudly on timeout so a hang
/// names what it was waiting for.
async fn wait_for<T>(
    socket: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + SinkExt<Message>
              + Unpin),
    what: &str,
    pick: impl Fn(&Value) -> Option<T>,
) -> T {
    let deadline = Duration::from_secs(60);
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
            if frame["type"] == "error" {
                panic!("gateway error while waiting for {what}: {frame}");
            }
            if let Some(found) = pick(&frame) {
                return found;
            }
        }
    };
    tokio::time::timeout(deadline, step)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
