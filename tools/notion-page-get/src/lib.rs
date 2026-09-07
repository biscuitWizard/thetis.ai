//! Read one Notion page: its properties and its content as markdown.
//!
//! Two calls behind one tool. `GET /v1/pages/{id}` returns the property values
//! and metadata; `GET /v1/pages/{id}/markdown` returns the body. They are
//! separate endpoints in the API and almost always wanted together, so joining
//! them here saves a round trip and, more to the point, saves the model from
//! reading a page and not noticing it never fetched the text.
//!
//! Markdown rather than the block API on purpose: the block endpoints return a
//! deeply nested tree that has to be walked recursively to see a whole page,
//! and costs far more context than the same content as markdown.
//!
//! Signed file URLs in the body are stripped of their AWS query string by the
//! shared client; see `shorten_signed_urls`.

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
            name: "notion-page-get".to_string(),
            description: "Read a Notion page: its properties, metadata, and its full content as \
                          markdown. Accepts a page id or a Notion URL. This is the tool for \
                          reading a page you already have an id for — use notion-search to find \
                          the id first."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "page_id": {
                        "type": "string",
                        "description": "Page id, dashed or bare, or the page URL copied from \
                                        Notion. A block id also works, and returns that block's \
                                        subtree."
                    },
                    "include_content": {
                        "type": "boolean",
                        "description": "Fetch the page body as markdown. Defaults to true. Set \
                                        false when you only need the properties, which is one \
                                        request instead of two."
                    },
                    "include_properties": {
                        "type": "boolean",
                        "description": "Fetch the page's property values. Defaults to true."
                    },
                    "include_transcript": {
                        "type": "boolean",
                        "description": "For meeting notes, include the full transcript rather \
                                        than a placeholder. Defaults to false."
                    }
                },
                "required": ["page_id"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;
        let page_id = notion::required_id(&args, "page_id")?;

        let want_content = flag(&args, "include_content", true);
        let want_properties = flag(&args, "include_properties", true);
        if !want_content && !want_properties {
            return Err(
                "include_content and include_properties are both false, so there is nothing to \
                 fetch. Leave at least one on."
                    .to_string(),
            );
        }

        let mut out = String::new();

        // The metadata call is what tells us the title and where the page
        // lives, so it goes first and its failure is the reported one: a 404
        // here is a wrong id or an unshared page, which is the usual cause.
        let page = if want_properties {
            let page = client.get(&format!("/v1/pages/{page_id}"), &[])?;
            out.push_str(&render_page(&page));
            Some(page)
        } else {
            None
        };

        if want_content {
            let mut query: Vec<(String, String)> = Vec::new();
            if flag(&args, "include_transcript", false) {
                query.push(("include_transcript".to_string(), "true".to_string()));
            }
            let markdown = client.get(&format!("/v1/pages/{page_id}/markdown"), &query)?;
            let body = notion::markdown_body(&markdown);

            out.push_str("\n--- content (markdown) ---\n");
            if body.trim().is_empty() {
                out.push_str("(this page has no content)\n");
            } else {
                out.push_str(&body);
                out.push('\n');
            }
        }

        if page.is_none() {
            out.push_str(&format!("\n(page id: {page_id})\n"));
        }
        Ok(out)
    }
}

fn flag(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn render_page(page: &Value) -> String {
    let mut out = format!("# {}\n", notion::title_of(page));

    if let Some(id) = page.get("id").and_then(Value::as_str) {
        out.push_str(&format!("id: {id}\n"));
    }
    if let Some(url) = page.get("url").and_then(Value::as_str) {
        out.push_str(&format!("url: {url}\n"));
    }
    if let Some(line) = notion::parent_line(page) {
        out.push_str(&format!("{line}\n"));
    }
    if let Some(edited) = page.get("last_edited_time").and_then(Value::as_str) {
        out.push_str(&format!("last edited: {edited}\n"));
    }
    if page.get("in_trash").and_then(Value::as_bool) == Some(true) {
        out.push_str("status: in trash\n");
    } else if page.get("archived").and_then(Value::as_bool) == Some(true) {
        out.push_str("status: archived\n");
    }
    if page.get("is_locked").and_then(Value::as_bool) == Some(true) {
        out.push_str("status: locked in the Notion UI (the API can still edit it)\n");
    }

    let properties = notion::describe_properties(page, "  ");
    if properties.trim().is_empty() {
        // A page whose parent is another page has only a title, so this is
        // normal rather than a problem.
        out.push_str("\nproperties: none set (pages outside a database have only a title)\n");
    } else {
        out.push_str("\nproperties (non-empty only):\n");
        out.push_str(&properties);
    }
    out
}

export!(Component);
