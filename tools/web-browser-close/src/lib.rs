//! Throw this session's browser context away.

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
            name: "web-browser-close".to_string(),
            description: "Close this conversation's browser context, discarding its cookies, \
                          storage and tabs. Rarely necessary — an idle context is reaped \
                          automatically and the shared browser keeps running — but it is the \
                          clean way to start a login flow from a known-empty state, or to \
                          drop a session that has got into a mess. The next navigation opens \
                          a fresh context."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "group:browser".to_string()],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let mut args = client::args(&args_json)?;
        // `all` would close every conversation's context, not just this one's.
        // Not exposed: no tool should be able to reach into another session.
        args.remove("all");
        client::call("close", &session_id, args, &config_json)
    }
}

export!(Component);
