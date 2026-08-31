//! Cookies, storage, dialogs and the viewport.

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
            name: "web-browser-state".to_string(),
            description: "Read and change the browser's own state, rather than the page's: \
                          cookies, localStorage, sessionStorage, the whole storage state, \
                          pending dialogs, and the viewport size. Pick what to work on with \
                          `kind` and what to do with `action` (default 'list'). Two common \
                          uses: resize with kind='viewport' to check a responsive layout, and \
                          arm kind='dialog' action='accept' *before* the click that triggers \
                          an alert or confirm, since a dialog blocks the page until answered."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["cookies", "localStorage", "sessionStorage", "storageState", "dialog", "viewport"],
                        "description": "Which state to work on. Defaults to cookies."
                    },
                    "action": {
                        "type": "string",
                        "enum": ["list", "get", "set", "delete", "clear", "accept", "dismiss"],
                        "description": "What to do. Defaults to 'list'. accept/dismiss are for kind='dialog'; viewport only resizes."
                    },
                    "name": {
                        "type": "string",
                        "description": "The cookie or storage key, for get, set and delete."
                    },
                    "value": {
                        "type": "string",
                        "description": "The value to store, for 'set'."
                    },
                    "domain": {
                        "type": "string",
                        "description": "Cookie domain, for set. Defaults to the current page's host."
                    },
                    "path": {
                        "type": "string",
                        "description": "Cookie path, for set. Defaults to '/'."
                    },
                    "promptText": {
                        "type": "string",
                        "description": "Text to type into a window.prompt before accepting it."
                    },
                    "width": { "type": "integer", "description": "Viewport width in pixels, with kind='viewport'." },
                    "height": { "type": "integer", "description": "Viewport height in pixels, paired with width." }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "group:browser".to_string()],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = client::args(&args_json)?;
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("cookies");
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        let named = args.get("name").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty());

        // Catch the combinations the sidecar would reject, or worse, silently
        // act on with an undefined key.
        if kind == "viewport" && !(args.contains_key("width") && args.contains_key("height")) {
            return Err("resizing the viewport needs `width` and `height`.".to_string());
        }
        if matches!(action, "set" | "delete" | "get")
            && matches!(kind, "cookies" | "localStorage" | "sessionStorage")
            && !named
        {
            return Err(format!("{action} on {kind} needs `name`."));
        }
        if action == "set" && !args.contains_key("value") {
            return Err("setting a value needs `value`. Pass an empty string to store one.".to_string());
        }
        client::call("state", &session_id, args, &config_json)
    }
}

export!(Component);
