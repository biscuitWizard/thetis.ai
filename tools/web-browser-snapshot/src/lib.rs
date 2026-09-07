//! Read the current page's accessibility tree.

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
            name: "web-browser-snapshot".to_string(),
            description: "Re-read the current page as an accessibility snapshot, with fresh \
                          `[ref=eN]` handles for clicking and typing. Navigation already \
                          returns one, so reach for this after the page changes under you — \
                          an expanded menu, a submitted form, a late-rendering widget — or \
                          when the refs you hold have gone stale, which they do on every \
                          reload. Filter with `text` or `regex` on a large page rather than \
                          pulling the whole tree."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Only lines containing this text, matched case-insensitively. The cheap way to find one control on a big page."
                    },
                    "regex": {
                        "type": "string",
                        "description": "Only lines matching this regular expression. Use instead of `text` when you need alternation or anchoring."
                    },
                    "context": {
                        "type": "integer",
                        "description": "Lines of surrounding context to keep around each match, like grep -C. Defaults to 0."
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
        client::call("snapshot", &session_id, args, &config_json)
    }
}

export!(Component);
