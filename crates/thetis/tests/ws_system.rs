//! A read-only check of the `system-status` frame against a running Thetis.
//!
//! The status bar polls this frame and draws whatever it says, so what matters
//! is that a real gateway answers it with real numbers — something no unit test
//! can show, since every interesting field comes from git, the loader, the
//! worker fleet or `/proc`. Ignored by default; sends nothing into any
//! conversation.
//!   THETIS_WS_URL=ws://127.0.0.1:7777/ws \
//!     cargo test -p thetis --test ws_system -- --ignored --nocapture

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn system_status_answers_with_real_facts() {
    let Some(url) = std::env::var("THETIS_WS_URL").ok() else {
        eprintln!("skipped: set THETIS_WS_URL");
        return;
    };

    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    socket
        .send(Message::Text(
            serde_json::json!({ "type": "system-status" }).to_string().into(),
        ))
        .await
        .unwrap();

    // The gateway pushes a catalogue and a session list on connect, so the
    // reply is not necessarily the first frame back.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let frame = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "no system-status frame within 10s");
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(remaining, socket.next()).await
        else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
        if frame["type"] == "system-status" {
            break frame;
        }
        // An older kernel routes the frame to the guest, which refuses it.
        if frame["type"] == "error" {
            panic!("host does not speak system-status: {}", frame["message"]);
        }
    };

    eprintln!("{}", serde_json::to_string_pretty(&frame).unwrap());

    assert_eq!(frame["ok"], true);
    // The five states the bar knows how to label. Anything else draws as
    // "unknown", which is a bug worth failing on here.
    let state = frame["state"].as_str().unwrap_or_default();
    assert!(
        ["running", "working", "building", "stale", "degraded"].contains(&state),
        "unexpected state {state:?}"
    );
    assert!(!frame["version"].as_str().unwrap_or_default().is_empty());
    assert_eq!(
        frame["wit"].as_str().unwrap_or_default().len(),
        16,
        "the WIT fingerprint should be 8 hex bytes"
    );

    // Trunk: a real 40-hex commit and a branch name, or this is not a checkout.
    let rev = frame["trunk"]["rev"].as_str().unwrap_or_default();
    assert_eq!(rev.len(), 40, "trunk rev is not a full commit id: {rev:?}");
    assert!(!frame["trunk"]["name"].as_str().unwrap_or_default().is_empty());

    // Which UI build is serving, judged against trunk's cache key.
    let serving = frame["ui"]["serving"].as_str().unwrap_or_default();
    assert!(
        ["current", "stale", "fallback", "unknown"].contains(&serving),
        "unexpected ui.serving {serving:?}"
    );

    // The machine. These come from /proc and are what the memory meter divides.
    let total = frame["host"]["mem_total_kb"].as_u64().expect("mem_total_kb");
    let available = frame["host"]["mem_available_kb"]
        .as_u64()
        .expect("mem_available_kb");
    assert!(total > 0 && available <= total);
    assert!(frame["host"]["cpus"].as_u64().unwrap_or(0) >= 1);
    assert!(frame["host"]["rss_kb"].as_u64().unwrap_or(0) > 0);
}
