//! Run JavaScript in the page.

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
            name: "web-browser-evaluate".to_string(),
            description: "Run JavaScript in the page and get its return value back as JSON. \
                          This is the escape hatch for what the accessibility tree does not \
                          show — a computed style, scroll position, the contents of a canvas \
                          or a shadow root, or an app's own state on `window`. Pass an \
                          expression or an arrow function; with `target` the element is \
                          handed to the function as its first argument. The value must be \
                          JSON-serialisable, so return a string or a plain object rather \
                          than a DOM node."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "function": {
                        "type": "string",
                        "description": "The JavaScript to run: an expression like 'document.title', or an arrow function like '() => window.scrollY'. With `target`, take the element as the argument: 'el => el.className'."
                    },
                    "target": {
                        "type": "string",
                        "description": "Optional element to pass into the function: a ref like 'e7' from a snapshot, or a CSS selector."
                    }
                },
                "required": ["function"],
                "additionalProperties": false
            })
            .to_string(),
            // Not read-only: arbitrary page script can submit forms and mutate
            // remote state as readily as a click can.
            capabilities: vec!["http".to_string(), "group:browser".to_string()],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = client::args(&args_json)?;
        if args.get("function").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
            return Err("evaluate needs `function`: the JavaScript to run.".to_string());
        }
        client::call("evaluate", &session_id, args, &config_json)
    }
}

export!(Component);
