//! Capture the page as an image or a PDF.

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
            name: "web-browser-screenshot".to_string(),
            description: "Capture the current page as an image, or as a PDF with \
                          action='pdf'. The file is written under the shared workspace and \
                          only its path comes back — a tool result is far too small to carry \
                          an image inline, so read it with the file tools or open the path in \
                          the UI. JPEG at quality 60 by default because it is much smaller; \
                          ask for PNG when you need exact pixels or transparency. Note that \
                          this shows you nothing directly: to check a layout, take the shot \
                          and then look at the file."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["pdf"],
                        "description": "Set to 'pdf' to print the page to PDF instead of capturing an image."
                    },
                    "target": {
                        "type": "string",
                        "description": "Capture just this element: a ref like 'e7' from a snapshot, or a CSS selector. Omit for the viewport."
                    },
                    "fullPage": {
                        "type": "boolean",
                        "description": "Capture the whole scrollable page rather than the visible viewport."
                    },
                    "type": {
                        "type": "string",
                        "enum": ["png", "jpeg"],
                        "description": "Image format. Defaults to jpeg, or png if `filename` ends in .png."
                    },
                    "quality": {
                        "type": "integer",
                        "description": "JPEG quality, 1-100. Defaults to 60. Ignored for PNG."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Basename for the file. Defaults to a timestamped name."
                    },
                    "format": {
                        "type": "string",
                        "description": "Paper size for a PDF, e.g. 'Letter' or 'A4'. Defaults to Letter."
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            // Writes a file into the workspace, so not read-only.
            capabilities: vec!["http".to_string(), "group:browser".to_string()],
        }
    }

    fn invoke(session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = client::args(&args_json)?;
        client::call("screenshot", &session_id, args, &config_json)
    }
}

export!(Component);
