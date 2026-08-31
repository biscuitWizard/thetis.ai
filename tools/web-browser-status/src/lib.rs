//! Is the headless browser up, and what is it doing?
//!
//! One of the `web-browser-*` family; see `src/client.rs` for why these are
//! HTTP clients rather than drivers.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod client;

use serde_json::json;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-browser-status".to_string(),
            description: "Check whether the headless browser is running, which Playwright \
                          version it is, and which conversations have a browser context \
                          open. Takes no arguments and starts nothing — the browser launches \
                          on the first real navigation and is dropped again when idle, so \
                          'running: false' here is normal, not a fault. Call this to tell a \
                          broken browser stack apart from one that is simply asleep."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec![
                "http".to_string(),
                "read-only".to_string(),
                "group:browser".to_string(),
            ],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = client::args(&args_json)?;
        client::call("status", &session_id, args, &config_json)
    }
}

export!(Component);
