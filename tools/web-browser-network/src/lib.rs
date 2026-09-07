//! What the page asked the network for.

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
            name: "web-browser-network".to_string(),
            description: "List the requests the page has made since the last navigation, as \
                          numbered lines with method, status and URL. Use `failedOnly` to go \
                          straight to the 404s and connection failures behind a missing \
                          image or an empty list, `filter` to match a URL by regex, and \
                          `index` to pull one request's full detail. The log holds the most \
                          recent 500 entries and is cleared on navigation."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "index": {
                        "type": "integer",
                        "description": "Show this one request in full, numbered as in the list (starts at 1)."
                    },
                    "filter": {
                        "type": "string",
                        "description": "Only requests whose URL or status matches this regular expression, case-insensitively."
                    },
                    "failedOnly": {
                        "type": "boolean",
                        "description": "Only requests that failed outright or returned 400 and above."
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
        client::call("network", &session_id, args, &config_json)
    }
}

export!(Component);
