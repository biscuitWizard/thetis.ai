//! Move the pointer over an element.

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
            name: "web-browser-hover".to_string(),
            description: "Hover the mouse over an element, to bring out what only appears \
                          under the pointer: a dropdown menu, a tooltip, a row's action \
                          buttons. Take a snapshot afterwards to see what appeared. Address \
                          the element by a `[ref=eN]` handle from the latest snapshot, a CSS \
                          selector, or x/y coordinates."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "What to hover: a ref like 'e7' from a snapshot, or a CSS selector. Omit only when giving x and y."
                    },
                    "x": { "type": "number", "description": "Viewport x coordinate, when there is no ref or selector to use." },
                    "y": { "type": "number", "description": "Viewport y coordinate, paired with x." }
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
        let has_target = args.get("target").and_then(|v| v.as_str()).is_some_and(|s| !s.trim().is_empty());
        let has_coords = args.contains_key("x") && args.contains_key("y");
        if !has_target && !has_coords {
            return Err("nothing to hover: give `target` (a ref like 'e7' from a snapshot, or \
                        a CSS selector), or both `x` and `y`."
                .to_string());
        }
        client::call("hover", &session_id, args, &config_json)
    }
}

export!(Component);
