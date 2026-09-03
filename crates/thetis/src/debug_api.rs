//! Inspection and turn-control frames the browser speaks.
//!
//! Handled host-side like `branch-*` and `workspace-*`, because both need
//! things the gateway guest cannot reach: the store, and the worker fleet.
//!
//! `debug-request` reads the conversation's last captured request out of the
//! store, where the worker put it (`host_api::capture_request`). Reading the
//! durable copy rather than asking the worker is what makes the inspector
//! answer at all after a restart — and it answers for stopped and archived
//! conversations too. `turn-cancel` does need the live worker, and settles for
//! saying "nothing running" when there is none: neither frame may spawn a
//! worker as a side effect of someone opening a panel.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::grip::{Grip, Role};

/// Frames answered on the socket without a reply-broadcast are kept under
/// this ceiling so a huge context cannot blow the WS message cap.
const MAX_REPLY_BYTES: usize = 12 * 1024 * 1024;

/// True when this frame type belongs here.
pub fn handles(frame_type: &str) -> bool {
    frame_type == "debug-request"
        || frame_type == "turn-cancel"
        || frame_type == "terminals"
        || frame_type == "terminal-close"
}

/// Handles one frame, returning the reply frames to send on this socket.
pub async fn handle(grip: &Arc<Grip>, frame: &Value) -> Vec<String> {
    let frame_type = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session = frame
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Which shell, for `terminal-close`. A separate field because `id` is
    // already spoken for by the conversation.
    let terminal = frame
        .get("terminal")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    match dispatch(grip, &frame_type, &session, &terminal).await {
        Ok(reply) => vec![reply],
        Err(e) => vec![
            json!({
                "type": frame_type,
                "session": session,
                "ok": false,
                "message": format!("{e:#}"),
            })
            .to_string(),
        ],
    }
}

async fn dispatch(
    grip: &Arc<Grip>,
    frame_type: &str,
    session: &str,
    terminal: &str,
) -> Result<String> {
    let Role::Gateway(router) = &grip.role else {
        anyhow::bail!("debug frames are a gateway concern");
    };
    anyhow::ensure!(!session.is_empty(), "missing 'id'");

    match frame_type {
        // The exact request body this conversation last sent to the provider.
        "debug-request" => {
            let stored = grip
                .persist
                .kv_get(session, crate::host_api::LAST_REQUEST_KEY)
                .await?;
            let Some(text) = stored.filter(|t| !t.is_empty()) else {
                return Ok(json!({
                    "type": "debug-request",
                    "session": session,
                    "ok": false,
                    "message": "no request captured for this conversation yet — \
                                send a message, then look again",
                })
                .to_string());
            };
            let captured: Value =
                serde_json::from_str(&text).context("the stored request is not readable")?;
            let reply = json!({
                "type": "debug-request",
                "session": session,
                "ok": true,
                "ts_ms": captured.get("ts_ms"),
                "body": captured.get("body"),
            })
            .to_string();
            anyhow::ensure!(
                reply.len() <= MAX_REPLY_BYTES,
                "the captured request is too large to ship over the socket ({} bytes)",
                reply.len()
            );
            Ok(reply)
        }

        // Stop the running turn. Politely a no-op when nothing is running.
        //
        // Deliberately never spawns a worker: a session with no live worker has
        // no turn to stop, so materializing one to tell it to do nothing would
        // be both slow and pointless.
        "turn-cancel" => {
            let Some(peer) = router.live_peer(session).await else {
                return Ok(json!({
                    "type": "turn-cancel",
                    "session": session,
                    "ok": true,
                    "stopped": false,
                    "message": "nothing running",
                })
                .to_string());
            };
            // The worker sets its stop flag synchronously before replying, so
            // by the time this returns the interruption is already visible to
            // every host call that conversation has in flight.
            let reply = peer.call("cancel", json!({ "session": session })).await?;
            let stopped = reply
                .get("stopped")
                .and_then(Value::as_bool)
                // An older worker answers `null`; it did do the cancel.
                .unwrap_or(true);
            Ok(json!({
                "type": "turn-cancel",
                "session": session,
                "ok": true,
                "stopped": stopped,
            })
            .to_string())
        }

        // The shells this conversation's sandbox is holding, each with its
        // transcript, so a tab that has just connected can draw them without
        // waiting for the next line of output.
        //
        // Never materializes a worker: a conversation with none has no shells
        // by definition, and the honest answer is the empty list.
        "terminals" => {
            let Some(peer) = router.live_peer(session).await else {
                return Ok(json!({
                    "type": "terminals",
                    "session": session,
                    "ok": true,
                    "terminals": [],
                })
                .to_string());
            };
            let reply = peer
                .call("terminals.list", json!({ "session": session }))
                .await?;
            Ok(json!({
                "type": "terminals",
                "session": session,
                "ok": true,
                "terminals": reply.get("terminals").cloned().unwrap_or(json!([])),
            })
            .to_string())
        }

        // Closing one shell from the drawer's trash button.
        //
        // Like `terminals`, this never materializes a worker: with none live
        // there is no shell to close, and spawning one in order to kill nothing
        // would be a surprising side effect of a delete button.
        "terminal-close" => {
            anyhow::ensure!(!terminal.is_empty(), "missing 'terminal'");
            let id = terminal;
            let Some(peer) = router.live_peer(session).await else {
                return Ok(json!({
                    "type": "terminal-close",
                    "session": session,
                    "ok": true,
                    "id": id,
                    "note": "that sandbox is not running, so it holds no shells",
                })
                .to_string());
            };
            let reply = peer
                .call("terminals.close", json!({ "session": session, "id": id }))
                .await?;
            Ok(json!({
                "type": "terminal-close",
                "session": session,
                "ok": reply.get("ok").and_then(|v| v.as_bool()).unwrap_or(true),
                "id": id,
                "note": reply.get("note").cloned().unwrap_or(json!(null)),
            })
            .to_string())
        }

        other => anyhow::bail!("unknown frame type: {other}"),
    }
}
