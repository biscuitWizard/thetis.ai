//! Add a comment to a Notion page, a block, or an existing discussion thread.
//!
//! `POST /v1/comments`. Exactly one target: a page, a block, or a discussion to
//! reply into. The API enforces that, and so does this tool — with an error that
//! names the choices, since "provide exactly one of three things" is easier to
//! fix when you are told which three.
//!
//! Comment markdown is inline-only. Headings, lists and code fences do not
//! become blocks inside a comment, so a caller writing a structured document
//! into a comment gets a wall of literal `#` characters. Worth saying in the
//! description rather than letting it be discovered.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-comment-add".to_string(),
            description: "Add a comment to a Notion page, to a specific block, or as a reply in \
                          an existing discussion thread (get discussion ids from \
                          notion-comment-list). Comment text supports inline markdown only — \
                          bold, italic, code, links — not headings, lists or code fences."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "page_id": {
                        "type": "string",
                        "description": "Comment on this page. Give exactly one of page_id, \
                                        block_id or discussion_id."
                    },
                    "block_id": {
                        "type": "string",
                        "description": "Comment on this specific block — a paragraph, heading or \
                                        to-do inside a page."
                    },
                    "discussion_id": {
                        "type": "string",
                        "description": "Reply into this existing discussion thread, from \
                                        notion-comment-list."
                    },
                    "text": {
                        "type": "string",
                        "description": "The comment body. Inline markdown only."
                    },
                    "display_name": {
                        "type": "string",
                        "description": "Author name to show instead of the connection's own \
                                        name. Optional."
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;

        let text = notion::required_str(&args, "text")?;
        let page = notion::optional_id(&args, "page_id")?;
        let block = notion::optional_id(&args, "block_id")?;
        let discussion = notion::optional_id(&args, "discussion_id")?;

        let targets = [&page, &block, &discussion]
            .iter()
            .filter(|t| t.is_some())
            .count();
        if targets != 1 {
            return Err(format!(
                "give exactly one target, not {targets}: page_id to comment on a page, block_id \
                 to comment on one block within a page, or discussion_id to reply to an existing \
                 thread (list them with notion-comment-list)."
            ));
        }

        // Markdown, not rich_text: the API accepts either, and markdown means a
        // caller does not have to build rich-text objects to write bold text.
        let mut body = json!({ "markdown": text });

        let where_to = if let Some(discussion) = &discussion {
            body["discussion_id"] = json!(discussion);
            format!("discussion {discussion}")
        } else if let Some(page) = &page {
            body["parent"] = json!({ "page_id": page });
            format!("page {page}")
        } else {
            let block = block.clone().unwrap_or_default();
            body["parent"] = json!({ "block_id": block });
            format!("block {block}")
        };

        if let Some(name) = notion::optional_str(&args, "display_name") {
            body["display_name"] = json!({ "type": "custom", "custom": { "name": name } });
        }

        let comment = client.post("/v1/comments", &body)?;

        let mut out = format!("Comment added to {where_to}.\n");
        if let Some(id) = comment.get("id").and_then(Value::as_str) {
            out.push_str(&format!("comment id: {id}\n"));
        }
        if let Some(id) = comment.get("discussion_id").and_then(Value::as_str) {
            out.push_str(&format!("discussion id: {id}\n"));
        }

        let written = notion::rich_text(comment.get("rich_text").unwrap_or(&Value::Null));
        if !written.trim().is_empty() {
            out.push_str(&format!("\ntext as stored: {}\n", notion::clip(&written, 500)));
        }
        Ok(out)
    }
}

export!(Component);
