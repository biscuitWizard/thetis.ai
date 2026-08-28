//! A read-only diagnostic: connect to a running Thetis, open one session,
//! and report which frames arrive for a while. Sends nothing into the
//! conversation. Ignored by default.
//!   THETIS_WS_URL=ws://127.0.0.1:7777/ws THETIS_PROBE_SESSION=<id> \
//!     THETIS_PROBE_SECS=20 \
//!     cargo test -p thetis --test ws_probe -- --ignored --nocapture

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn watch_a_session() {
    let (Some(url), Some(session)) = (
        std::env::var("THETIS_WS_URL").ok(),
        std::env::var("THETIS_PROBE_SESSION").ok(),
    ) else {
        eprintln!("skipped: set THETIS_WS_URL and THETIS_PROBE_SESSION");
        return;
    };
    let secs: u64 = std::env::var("THETIS_PROBE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    socket
        .send(Message::Text(
            serde_json::json!({ "type": "open", "id": session }).to_string().into(),
        ))
        .await
        .unwrap();

    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(next) = tokio::time::timeout(remaining, socket.next()).await else {
            break;
        };
        let Some(Ok(Message::Text(text))) = next else { continue };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
        let ty = frame["type"].as_str().unwrap_or("?").to_string();
        let kind = frame["kind"].as_str().unwrap_or("").to_string();
        let label = if kind.is_empty() { ty } else { format!("{ty}/{kind}") };
        *counts.entry(label.clone()).or_default() += 1;
        if counts[&label] <= 2 && label != "event/delta" {
            let short: String = text.chars().take(220).collect();
            eprintln!("first {label}: {short}");
        }
    }
    eprintln!("--- frame counts over {secs}s ---");
    for (label, n) in counts {
        eprintln!("{n:>5}  {label}");
    }
}
