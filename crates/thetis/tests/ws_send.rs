//! Operational tool: send one or more frames to a running Thetis over the
//! real websocket, printing every reply for a short window. Ignored by
//! default; drives the same protocol the browser does.
//!   THETIS_WS_URL=ws://127.0.0.1:7777/ws \
//!   THETIS_SEND_FRAMES='[{"type":"list"}]' \
//!     cargo test -p thetis --test ws_send -- --ignored --nocapture

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn send_frames() {
    let (Some(url), Some(frames)) = (
        std::env::var("THETIS_WS_URL").ok(),
        std::env::var("THETIS_SEND_FRAMES").ok(),
    ) else {
        eprintln!("skipped: set THETIS_WS_URL and THETIS_SEND_FRAMES");
        return;
    };
    let frames: Vec<Value> = serde_json::from_str(&frames).expect("frames must be a JSON array");

    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    for frame in &frames {
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
        eprintln!("-> {frame}");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let short: String = text.chars().take(300).collect();
                eprintln!("<- {short}");
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
}
