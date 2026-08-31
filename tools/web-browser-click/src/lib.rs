//! Click something on the page.

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
            name: "web-browser-click".to_string(),
            description: "Click an element on the current page. Address it by a `[ref=eN]` \
                          handle from the latest snapshot, which is the reliable way, or by \
                          CSS selector, or by raw x/y coordinates as a last resort. Refs \
                          belong to the snapshot that produced them: after the page \
                          navigates or re-renders, take a fresh snapshot rather than reusing \
                          an old ref. Returns the page context after the click, so a \
                          navigation it triggered is visible in the result."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "What to click: a ref like 'e7' from a snapshot, or a CSS selector. Omit only when giving x and y."
                    },
                    "x": { "type": "number", "description": "Viewport x coordinate, when there is no ref or selector to use." },
                    "y": { "type": "number", "description": "Viewport y coordinate, paired with x." },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "Mouse button. Defaults to left."
                    },
                    "doubleClick": { "type": "boolean", "description": "Double-click instead of a single click." },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["Alt", "Control", "Meta", "Shift"] },
                        "description": "Modifier keys to hold down, e.g. ['Control'] to open a link in a new tab."
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "group:browser".to_string()],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = client::args(&args_json)?;
        // Clicking nothing in particular is always a mistake worth naming here,
        // rather than letting the sidecar guess at the page origin.
        let has_target = args.get("target").and_then(|v| v.as_str()).is_some_and(|s| !s.trim().is_empty());
        let has_coords = args.contains_key("x") && args.contains_key("y");
        if !has_target && !has_coords {
            return Err("nothing to click: give `target` (a ref like 'e7' from a snapshot, or \
                        a CSS selector), or both `x` and `y`."
                .to_string());
        }
        client::call("click", &session_id, args, &config_json)
    }
}

export!(Component);
