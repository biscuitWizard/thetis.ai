//! The Thetis web chat gateway.
//!
//! Owns the whole user-facing surface: the single-page app and the JSON wire
//! protocol spoken over the websocket. The orchestrator owns only the socket,
//! so everything here — layout, styling, protocol — can be rewritten and
//! hot-swapped while people are connected.
//!
//! The pieces:
//!   `assets`   the embedded app, one table entry per file
//!   `handlers` one function per client action
//!   `render`   session events to wire frames

wit_bindgen::generate!({
    world: "gateway",
    path: "../../wit",
    generate_all,
    additional_derives: [serde::Serialize],
});

mod assets;
mod handlers;
mod render;

use serde_json::Value;

struct Component;

impl Guest for Component {
    fn describe() -> GatewayManifest {
        GatewayManifest {
            name: "web".to_string(),
            version_note: "multi-session chat: streaming, attachments, mode and model pickers"
                .to_string(),
        }
    }

    fn serve_asset(path: String) -> Option<Asset> {
        assets::find(&path).map(|a| Asset {
            mime: a.mime.to_string(),
            bytes: a.body.as_bytes().to_vec(),
        })
    }

    fn on_client_message(_client_id: String, frame_json: String) -> Vec<GatewayAction> {
        match serde_json::from_str::<Value>(&frame_json) {
            Ok(frame) => handlers::dispatch(&frame),
            Err(e) => vec![handlers::error(format!("malformed frame: {e}"))],
        }
    }

    fn render_event(event: OutboundEvent) -> Option<String> {
        render::event(&event).map(|v| v.to_string())
    }
}

export!(Component);
