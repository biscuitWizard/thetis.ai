//! Temporary operational tool: ping sessions' workers over the real WS so the
//! idle reaper sees activity. Used while deploying the reaper fix; harmless
//! otherwise. Ignored by default.
//!   THETIS_WS_URL=... THETIS_KEEPALIVE_SESSIONS=id1,id2 \
//!   THETIS_KEEPALIVE_SECS=600 cargo test -p thetis --test ws_keepalive -- --ignored

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn keep_workers_alive() {
    let (Some(url), Some(sessions)) = (
        std::env::var("THETIS_WS_URL").ok(),
        std::env::var("THETIS_KEEPALIVE_SESSIONS").ok(),
    ) else {
        eprintln!("skipped");
        return;
    };
    let secs: u64 = std::env::var("THETIS_KEEPALIVE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let sessions: Vec<String> = sessions.split(',').map(str::to_string).collect();

    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        for session in &sessions {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({ "type": "branch-status", "id": session })
                        .to_string()
                        .into(),
                ))
                .await;
        }
        // Drain replies so the socket does not back up.
        let drain_until = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < drain_until {
            let left = drain_until.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, socket.next()).await {
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        eprintln!("pinged {} session(s)", sessions.len());
        tokio::time::sleep(Duration::from_secs(120)).await;
    }
}
