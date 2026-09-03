//! What the page logged.

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
            name: "web-browser-console".to_string(),
            description: "Read the console messages and uncaught page errors collected since \
                          the last navigation. This is where the reason a page looks broken \
                          usually is — a failed import, a thrown exception, a framework \
                          warning — so check it before theorising about a blank screen. \
                          Filter with `level` to cut the noise; navigating to a new URL \
                          clears the history."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "level": {
                        "type": "string",
                        "enum": ["debug", "log", "info", "warning", "error", "pageerror"],
                        "description": "Only messages at this level or more severe. 'error' is the usual choice."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many messages to return, most recent last. Defaults to 100 of the 500 kept; each message is itself cut at 2000 chars."
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
        client::call("console", &session_id, args, &config_json)
    }
}

export!(Component);
