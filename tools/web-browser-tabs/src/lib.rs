//! List the open tabs, or open, switch to and close one.
//!
//! A Thetis tool component. `describe` tells the model what this tool is and
//! what arguments it takes; `invoke` does the work. Edit this file with
//! `write_code` or `patch_code` — every edit rebuilds and reloads immediately,
//! and the compiler's output comes back in the tool result.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

// `tool-manifest` is already in scope from the world's own `use types.{...}`;
// anything else has to be imported from the types interface.
use thetis::grip::sys;
use thetis::grip::types::LogLevel;
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-browser-tabs".to_string(),
            description: "List the open tabs, or open, switch to and close one.".to_string(),
            // Must be a JSON Schema object: it becomes the tool's parameter
            // definition in the model's tool list.
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "What to work on."
                    }
                },
                "required": ["input"],
                "additionalProperties": false
            })
            .to_string(),
            // Host capabilities this tool needs, e.g. "sandbox".
            capabilities: vec![],
        }
    }

    /// `config_json` is this tool's own `[tools.web-browser-tabs]` block from
    /// thetis.toml, or `{}` when it has none. Settings a tool needs — an API
    /// key, an endpoint, a default — belong there rather than hardcoded here.
    fn invoke(
        _session_id: String,
        args_json: String,
        config_json: String,
    ) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let config: Value = serde_json::from_str(&config_json).unwrap_or(json!({}));
        let _ = &config;

        let input = args
            .get("input")
            .and_then(Value::as_str)
            .ok_or("missing required argument 'input'")?;

        sys::log(LogLevel::Debug, &format!("web-browser-tabs invoked with: {input}"));

        // Replace this with the real implementation.
        Ok(format!("web-browser-tabs is a stub; it received: {input}"))
    }
}

export!(Component);
