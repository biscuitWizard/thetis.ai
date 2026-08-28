//! List the unresolved comments on a Notion page or block.
//!
//! `GET /v1/comments`. Comments are grouped by discussion thread, because a
//! reply only makes sense against a thread: notion-comment-add takes a
//! discussion_id to reply into an existing conversation, and that id is only
//! discoverable here.
//!
//! Author names come from one attempt at the workspace user listing, which a
//! personal access token is refused; ids are shortened when that happens.
//!
//! Note that the endpoint returns *unresolved* comments only. Resolved ones are
//! not available through the API at all, so an empty result does not mean
//! nobody ever commented.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 100;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-comment-list".to_string(),
            description: "List the unresolved comments on a Notion page or block, grouped by \
                          discussion thread. The discussion ids shown here are what \
                          notion-comment-add needs in order to reply to a thread rather than \
                          start a new one. Resolved comments are not exposed by the API."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "page_id": {
                        "type": "string",
                        "description": "The page (or block) whose comments to list. A page URL \
                                        works too."
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!("Maximum comments to gather, 1-{MAX_LIMIT}. \
                                                Defaults to {DEFAULT_LIMIT}.")
                    },
                    "start_cursor": {
                        "type": "string",
                        "description": "Resume from a cursor returned by an earlier call."
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
        let want = notion::limit(&args, DEFAULT_LIMIT, MAX_LIMIT);

        let mut base = json!({ "block_id": page_id });
        if let Some(cursor) = notion::optional_str(&args, "start_cursor") {
            base["start_cursor"] = json!(cursor);
        }

        let (comments, next) = client.paginate("GET", "/v1/comments", &base, want)?;

        if comments.is_empty() {
            return Ok(format!(
                "No unresolved comments on {page_id}.\n\n\
                 The API only exposes unresolved comments, so resolved threads would not appear \
                 here. If you expected comments and the connection is new, check that it has \
                 comment read capability — that is off by default."
            ));
        }

        Ok(render(&client, &comments, next.as_ref(), &page_id))
    }
}

/// Renders threads, resolving author ids to names first.
fn render(client: &Notion, comments: &[Value], next: Option<&String>, page_id: &str) -> String {
    // Group by discussion, preserving the order threads first appear so the
    // output is stable between calls.
    let mut threads: Vec<(String, Vec<&Value>)> = Vec::new();
    for comment in comments {
        let discussion = comment
            .get("discussion_id")
            .and_then(Value::as_str)
            .unwrap_or("(no discussion id)")
            .to_string();
        match threads.iter_mut().find(|(id, _)| *id == discussion) {
            Some((_, bucket)) => bucket.push(comment),
            None => threads.push((discussion, vec![comment])),
        }
    }

    // Comments carry only an author id. One attempt at the workspace user
    // listing turns those into names; a personal access token is refused it,
    // in which case ids are shortened for display instead.
    let names = notion::resolve_user_names(client);
    let names_unavailable = names.is_empty();

    let mut out = format!(
        "{} unresolved comment(s) on {page_id}, in {} thread(s).\n",
        comments.len(),
        threads.len()
    );

    for (discussion, thread) in &threads {
        out.push_str(&format!("\n--- discussion {discussion} ---\n"));
        for comment in thread {
            let author = notion::user_label(
                comment.get("created_by").unwrap_or(&Value::Null),
                &names,
            );
            let when = comment
                .get("created_time")
                .and_then(Value::as_str)
                .unwrap_or("");

            out.push_str(&format!("{author} at {when}:\n"));

            let text = notion::rich_text(comment.get("rich_text").unwrap_or(&Value::Null));
            if text.trim().is_empty() {
                out.push_str("  (empty comment)\n");
            } else {
                for line in notion::clip(text.trim(), 1500).lines() {
                    out.push_str(&format!("  {line}\n"));
                }
            }

            if let Some(id) = comment.get("id").and_then(Value::as_str) {
                out.push_str(&format!("  comment id: {id}\n"));
            }
            if let Some(attachments) = comment.get("attachments").and_then(Value::as_array) {
                if !attachments.is_empty() {
                    out.push_str(&format!("  {} attachment(s)\n", attachments.len()));
                }
            }
        }
        out.push_str(&format!(
            "reply into this thread: notion-comment-add with discussion_id {discussion}\n"
        ));
    }

    if names_unavailable {
        out.push_str(
            "\nAuthors show as ids: this token cannot read the workspace user list \
             (a personal access token may only look itself up). An integration token with \
             user-information capability would show names.\n",
        );
    }
    out.push_str(&notion::pagination_note(comments.len(), next));
    out
}

export!(Component);
