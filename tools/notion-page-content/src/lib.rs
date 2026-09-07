//! Edit a Notion page's body as markdown.
//!
//! `PATCH /v1/pages/{id}/markdown`. Three ways to change content, and the
//! choice matters:
//!
//! - `edit` — search-and-replace on the existing text. The recommended one:
//!   surgical, and it fails loudly when the text it expects is not there,
//!   rather than quietly writing over something else.
//! - `append` / `prepend` — add without touching what is already there.
//! - `replace` — throw away the whole body and write a new one.
//!
//! `replace` is the destructive one, so it refuses to run unless
//! `confirm_replace` is set. That is not ceremony: replacing a page is the sort
//! of thing an agent does by reflex when the user asked to add a paragraph, and
//! Notion's own API has no undo.

// Request shapes, confirmed against the live API rather than inferred — the
// three commands disagree with each other and the docs do not spell all of it
// out. Every body needs a top-level `type` discriminator naming the command:
//
//   update_content   { "content_updates": [ { old_str, new_str } ] }
//   replace_content  { "new_str": "..." }
//   insert_content   { "content":  "...", "position": { "type": "start"|"end" } }
//
// Note `update_content` takes `content_updates` (not `operations`), and
// `insert_content` takes `content` while `replace_content` takes `new_str`.
// Getting one wrong yields `validation_error: body.<field> should be defined`,
// which names the field it wanted — worth reading closely.

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
            name: "notion-page-content".to_string(),
            description: "Change the body of a Notion page using markdown. Modes: 'edit' \
                          (search-and-replace exact text — preferred), 'append', 'prepend', or \
                          'replace' (overwrites the whole page; needs confirm_replace). Read the \
                          page with notion-page-get first so the text you search for is exact."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "page_id": {
                        "type": "string",
                        "description": "Page id, or the page URL copied from Notion."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["edit", "append", "prepend", "replace"],
                        "description": "How to apply the change. Defaults to 'append'. Use \
                                        'edit' for targeted changes."
                    },
                    "content": {
                        "type": "string",
                        "description": "Markdown to add, for append/prepend/replace."
                    },
                    "edits": {
                        "type": "array",
                        "description": "For mode 'edit': the replacements to make. Each old_text \
                                        must appear in the page exactly, and by default must be \
                                        unique.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Existing text to find, copied exactly."
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "Replacement markdown. An empty string \
                                                    deletes the matched text."
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "description": "Replace every occurrence rather than \
                                                    failing when old_text matches more than once."
                                }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    },
                    "confirm_replace": {
                        "type": "boolean",
                        "description": "Required for mode 'replace', which discards the page's \
                                        existing content. Notion has no undo for this."
                    },
                    "allow_deleting_content": {
                        "type": "boolean",
                        "description": "Permit the edit to remove child pages or databases. Off \
                                        by default: Notion refuses such an edit rather than \
                                        destroying nested content."
                    }
                },
                "required": ["page_id"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;
        let page_id = notion::required_id(&args, "page_id")?;

        let mode = notion::optional_str(&args, "mode").unwrap_or_else(|| "append".to_string());
        let allow_deleting = args
            .get("allow_deleting_content")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let (command, summary) = match mode.as_str() {
            "edit" => {
                let edits = args
                    .get("edits")
                    .and_then(Value::as_array)
                    .filter(|a| !a.is_empty())
                    .ok_or(
                        "mode 'edit' needs a non-empty 'edits' array of {old_text, new_text}.",
                    )?;

                let mut operations = Vec::new();
                for (i, edit) in edits.iter().enumerate() {
                    let old = edit
                        .get("old_text")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            format!("edits[{i}] has no 'old_text'; it must be text to search for")
                        })?;
                    let new = edit
                        .get("new_text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("edits[{i}] has no 'new_text'"))?;

                    let mut operation = json!({ "old_str": old, "new_str": new });
                    if edit.get("replace_all").and_then(Value::as_bool) == Some(true) {
                        operation["replace_all_matches"] = json!(true);
                    }
                    operations.push(operation);
                }

                let count = operations.len();
                let mut command = json!({ "content_updates": operations });
                if allow_deleting {
                    command["allow_deleting_content"] = json!(true);
                }
                (
                    json!({ "type": "update_content", "update_content": command }),
                    format!("applied {count} edit(s)"),
                )
            }
            "replace" => {
                if args.get("confirm_replace").and_then(Value::as_bool) != Some(true) {
                    return Err(
                        "mode 'replace' discards everything currently on the page and Notion has \
                         no undo. Set confirm_replace: true if that is really the intent — or use \
                         mode 'edit' to change part of the page, or 'append' to add to it."
                            .to_string(),
                    );
                }
                let content = notion::required_str(&args, "content")?;
                let mut command = json!({ "new_str": content });
                if allow_deleting {
                    command["allow_deleting_content"] = json!(true);
                }
                (
                    json!({ "type": "replace_content", "replace_content": command }),
                    "replaced the whole page body".to_string(),
                )
            }
            "prepend" => {
                let content = notion::required_str(&args, "content")?;
                (
                    json!({
                        "type": "insert_content",
                        "insert_content": {
                            "content": content,
                            "position": { "type": "start" }
                        }
                    }),
                    "inserted at the top of the page".to_string(),
                )
            }
            "append" => {
                let content = notion::required_str(&args, "content")?;
                (
                    json!({
                        "type": "insert_content",
                        "insert_content": {
                            "content": content,
                            "position": { "type": "end" }
                        }
                    }),
                    "appended to the end of the page".to_string(),
                )
            }
            other => {
                return Err(format!(
                    "unknown mode {other:?}. Use 'edit', 'append', 'prepend' or 'replace'."
                ))
            }
        };

        let response = client.patch(&format!("/v1/pages/{page_id}/markdown"), &command)?;

        let mut out = format!("Page {page_id}: {summary}.\n");
        out.push_str("\n--- page content now (markdown) ---\n");
        out.push_str(&notion::markdown_body_preview(&response));
        out.push('\n');
        Ok(out)
    }
}

export!(Component);
