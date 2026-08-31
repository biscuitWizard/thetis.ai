//! List and manage the session's tabs.

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
            name: "web-browser-tabs".to_string(),
            description: "List this session's tabs, or open, switch to and close one. Called \
                          with no arguments it just lists them, marking the active one with \
                          '*' — which is how to get the index the other actions take. Every \
                          other browser tool acts on the active tab, so switching here \
                          redirects all of them."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["new", "select", "close"],
                        "description": "What to do. Omit to just list the tabs."
                    },
                    "index": {
                        "type": "integer",
                        "description": "Which tab, zero-based, as shown in the listing. Required by 'select'; 'close' defaults to the active tab."
                    },
                    "url": {
                        "type": "string",
                        "description": "For 'new': navigate the new tab here after opening it."
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
        if args.get("action").and_then(|v| v.as_str()) == Some("select")
            && !args.contains_key("index")
        {
            return Err("selecting a tab needs `index` — call this with no arguments first to \
                        see the list."
                .to_string());
        }
        client::call("tabs", &session_id, args, &config_json)
    }
}

export!(Component);
