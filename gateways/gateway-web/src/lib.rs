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

use crate::thetis::grip::sys;

struct Component;

/// Replaced in served assets with the configured agent name.
///
/// The distinction this keeps: *Thetis* is the harness — the process, the
/// branch machinery, the version in the status bar — while the agent that
/// talks to you is named by `agent.name` and defaults to Thetis too. Only
/// text that refers to the agent carries this placeholder.
const AGENT_NAME_PLACEHOLDER: &str = "{agent_name}";

/// Fills in the agent's identity in an HTML asset.
///
/// The avatar is expressed as two placeholders rather than one because the
/// markup holds both an `<img>` and the built-in `<svg>` mark, and exactly one
/// is shown. Toggling a `hidden` attribute is the house rule for hiding, and it
/// keeps the fallback available to the client: if the image 404s, `app.js` can
/// swap back to the mark without asking the host for new markup.
fn fill_identity(body: &str) -> String {
    let name = sys::config_get("agent_name")
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Thetis".to_string());

    // Empty is a real choice here, not a missing value: it selects the built-in
    // mark rather than meaning "unset, go and find a default".
    let avatar = sys::config_get("agent_avatar").unwrap_or_default();
    let avatar = avatar.trim();
    let has_avatar = !avatar.is_empty();

    body.replace(AGENT_NAME_PLACEHOLDER, name.trim())
        // A quote in a configured URL would otherwise break out of the
        // attribute it sits in, so it is escaped rather than trusted.
        .replace("{agent_avatar}", &avatar.replace('"', "%22"))
        .replace(
            "{agent_avatar_hidden}",
            if has_avatar { "" } else { "hidden" },
        )
        .replace("{agent_mark_hidden}", if has_avatar { "hidden" } else { "" })
}

impl Guest for Component {
    fn describe() -> GatewayManifest {
        GatewayManifest {
            name: "web".to_string(),
            version_note: "multi-session chat: streaming, attachments, mode and model pickers"
                .to_string(),
        }
    }

    fn serve_asset(path: String) -> Option<Asset> {
        assets::find(&path).map(|a| {
            // Only HTML carries the placeholder today, so the substitution is
            // limited to it rather than run over every stylesheet and the
            // vendored terminal emulator.
            let body = if a.mime.starts_with("text/html") {
                fill_identity(a.body)
            } else {
                a.body.to_string()
            };
            Asset {
                mime: a.mime.to_string(),
                bytes: body.into_bytes(),
            }
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
