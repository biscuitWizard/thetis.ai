//! Wait for the page to catch up.

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
            name: "web-browser-wait".to_string(),
            description: "Wait for the page to reach a state before going on: text to appear \
                          or disappear, an element to become visible or detached, or loading \
                          to finish. Every other browser tool already waits for the page to \
                          settle, so reach for this only when that is not enough — a spinner \
                          to clear, a toast to vanish, a slow XHR to land. Prefer waiting on \
                          text or an element over a fixed sleep, which is slower and still \
                          races."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Wait until this text is visible on the page." },
                    "textGone": { "type": "string", "description": "Wait until this text is no longer visible — a spinner or a toast." },
                    "target": {
                        "type": "string",
                        "description": "Wait for this element: a ref like 'e7' from a snapshot, or a CSS selector."
                    },
                    "state": {
                        "type": "string",
                        "enum": ["visible", "hidden", "attached", "detached"],
                        "description": "Which state `target` must reach. Defaults to visible."
                    },
                    "loadState": {
                        "type": "string",
                        "enum": ["load", "domcontentloaded", "networkidle"],
                        "description": "Wait for this load state. 'networkidle' is the one for a page that renders itself after load."
                    },
                    "time": {
                        "type": "number",
                        "description": "Wait this many milliseconds unconditionally. The last resort — prefer a condition."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "How long to wait before giving up, in milliseconds. Defaults to the sidecar's own timeout (15000)."
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
        let args = client::args(&args_json)?;
        // The sidecar checks these in order and falls through to a bare settle,
        // which would look like a silent no-op to the caller.
        const CONDITIONS: &[&str] = &["text", "textGone", "target", "loadState", "time"];
        if !CONDITIONS.iter().any(|k| args.contains_key(*k)) {
            return Err("nothing to wait for: give one of `text`, `textGone`, `target`, \
                        `loadState` or `time`."
                .to_string());
        }
        client::call("wait", &session_id, args, &config_json)
    }
}

export!(Component);
