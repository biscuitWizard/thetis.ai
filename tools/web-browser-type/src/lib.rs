//! Put text into the page: fields, keys, select options.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod client;

use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-browser-type".to_string(),
            description: "Type into the page. Four modes: `fill` (the default) puts `text` \
                          into the element at `target`; `press_key` sends one key such as \
                          'Enter' or 'ArrowDown' to the focused element; `fill_form` fills \
                          several fields in one call, which is much better than a call each; \
                          `select_option` picks `values` in a <select>. Targets are `[ref=eN]` \
                          handles from a snapshot or CSS selectors. Set `submit` to press \
                          Enter after filling."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["fill", "press_key", "fill_form", "select_option"],
                        "description": "Which mode. Defaults to 'fill'."
                    },
                    "target": {
                        "type": "string",
                        "description": "The element: a ref like 'e7' from a snapshot, or a CSS selector. Needed by every mode except press_key."
                    },
                    "text": { "type": "string", "description": "The text to type, for 'fill'." },
                    "key": {
                        "type": "string",
                        "description": "The key to press, for 'press_key' — e.g. 'Enter', 'Tab', 'Escape', 'ArrowDown', 'Control+a'."
                    },
                    "fields": {
                        "type": "array",
                        "description": "For 'fill_form': the fields to fill, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": { "type": "string", "description": "Ref or CSS selector for this field." },
                                "value": { "type": "string", "description": "What to put in it." },
                                "type": {
                                    "type": "string",
                                    "enum": ["text", "checkbox", "radio", "select"],
                                    "description": "Field kind, when it is not a plain text input. Defaults to text."
                                }
                            },
                            "required": ["target", "value"],
                            "additionalProperties": false
                        }
                    },
                    "values": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "For 'select_option': the option values or labels to select. A single-select takes one."
                    },
                    "slowly": {
                        "type": "boolean",
                        "description": "Type character by character instead of setting the value at once. Slower, but it fires the keystroke handlers an autocomplete needs."
                    },
                    "submit": { "type": "boolean", "description": "Press Enter after filling, to submit the form." }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "group:browser".to_string()],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let mut args = client::args(&args_json)?;
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("fill")
            .to_string();

        // Each mode has one argument it cannot work without, and saying so here
        // beats a Playwright timeout on an empty selector.
        match action.as_str() {
            "press_key" => {
                if args.get("key").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
                    return Err("press_key needs `key`, e.g. 'Enter' or 'ArrowDown'.".to_string());
                }
            }
            "fill_form" => {
                if !args.get("fields").is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
                    return Err("fill_form needs a non-empty `fields` array of {target, value}.".to_string());
                }
            }
            "select_option" => {
                if !args.get("values").is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
                    return Err("select_option needs `values`, the option values to select.".to_string());
                }
                if args.get("target").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
                    return Err("select_option needs `target`, the <select> to pick in.".to_string());
                }
            }
            "fill" => {
                if args.get("target").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
                    return Err("fill needs `target`, the field to type into.".to_string());
                }
                if !args.contains_key("text") {
                    return Err("fill needs `text`. Pass an empty string to clear the field.".to_string());
                }
                // 'fill' is this tool's own default, not one the sidecar knows;
                // it treats an absent action as a plain fill.
                args.remove("action");
            }
            other => {
                return Err(format!(
                    "unknown action '{other}'. Use fill, press_key, fill_form or select_option."
                ));
            }
        }
        if action != "fill" {
            args.insert("action".to_string(), Value::String(action));
        }
        client::call("type", &session_id, args, &config_json)
    }
}

export!(Component);
