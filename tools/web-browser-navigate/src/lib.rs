//! Drive the headless browser to a URL, or go back, forward or reload.
//!
//! One of the `web-browser-*` family. These are thin HTTP clients to the
//! Playwright sidecar the kernel supervises (`crates/thetis/src/browser.rs`):
//! tool components are wasm and cannot spawn a process, so nothing here can
//! drive a browser directly, but `wasi:http` reaches loopback. The sidecar does
//! all the formatting, so the whole family stays consistent and a change to
//! output shape does not mean recompiling fourteen components.

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
            name: "web-browser-navigate".to_string(),
            description: "Point the headless browser at a URL and get back the page's \
                          accessibility snapshot — the tree of roles, names and `[ref=eN]` \
                          handles that every other web-browser-* tool addresses elements by. \
                          Also goes back, forward or reloads. This is normally the first \
                          browser call you make: the refs it returns are what \
                          web-browser-click and web-browser-type take as their `target`. \
                          Each conversation gets its own browser context, so cookies and \
                          storage are not shared with anyone else. Reaches the network."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to open. Required unless `action` is set."
                    },
                    "action": {
                        "type": "string",
                        "enum": ["goto", "back", "forward", "reload"],
                        "description": "What to do. Defaults to 'goto', which needs a `url`. \
                                        The history actions ignore `url`."
                    },
                    "waitUntil": {
                        "type": "string",
                        "enum": ["load", "domcontentloaded", "networkidle", "commit"],
                        "description": "When to consider navigation finished. Defaults to \
                                        'load'. Use 'networkidle' for a page that renders \
                                        itself after load, 'commit' to not wait at all."
                    }
                },
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
        let mut args = client::args(&args_json)?;
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("goto")
            .to_string();
        if action == "goto" && args.get("url").and_then(|v| v.as_str()).is_none() {
            return Err("navigating needs a `url`, or an `action` of back, forward or reload."
                .to_string());
        }
        // The sidecar treats a missing/unknown action as a goto, and only looks
        // at `action` for the history moves.
        if action == "goto" {
            args.remove("action");
        }
        client::call("navigate", &session_id, args, &config_json)
    }
}

export!(Component);
